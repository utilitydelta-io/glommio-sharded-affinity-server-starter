use core::fmt;
use std::{cell::RefCell, num::NonZeroUsize, os::fd::{IntoRawFd, FromRawFd}, rc::Rc, vec};

use glommio::{channels::channel_mesh::{Full, MeshBuilder, Senders}, enclose, net::TcpListener, spawn_local, CpuSet, LocalExecutorPoolBuilder, PoolPlacement};
use futures_lite::AsyncReadExt;
use futures_lite::AsyncWriteExt;
use log::{debug, error, info};
use lru::LruCache;

struct Msg {
    fd: i32,
    value: u64,
}

impl fmt::Display for Msg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Msg(fd: {}, value: {})", self.fd, self.value)
    }
}

fn compute_fibonacci(n: u64) -> u64 {
    if n <= 1 {
        return n as u64;
    }
    
    // CPU intensive iterative approach with extra work
    let mut a = 0u64;
    let mut b = 1u64;
    
    for i in 2..=n {
        let temp = a.wrapping_add(b);
        a = b;
        b = temp;
        
        // Add some extra CPU work to make it more intensive
        if i % 1000 == 0 {
            // Do some meaningless computation every 1000 iterations
            let _extra_work: u64 = (0..100).map(|x| x * x).sum();
        }
    }
    
    b
}

fn main() {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info) // Set default level
        .init();
    
    info!("Hello, world! Starting sharded TCP server using Glommio...");
    

    // Size of the LRU cache for Fibonacci results, per shard
    let lru_cache_size = 10000;

    // Take advantage of all available CPUs
    let nbr_shards = num_cpus::get();
    let online_cpus = CpuSet::online().ok();
    info!("Number of CPUs: {nbr_shards}, Online CPUs: {online_cpus:?}");

    // A full mesh channel is required to allow any shard to communicate with any other shard without locking primitives
    let mesh_channel_size = 1024;
    let mesh = MeshBuilder::<Msg, Full>::full(nbr_shards, mesh_channel_size);

    // Create a pool of executors, one per shard
    // Each executor will run threads pinned to a single core
    // The mesh BUILDER is copied in to each shard so they can all join the full mesh
    LocalExecutorPoolBuilder::new(PoolPlacement::MaxSpread(
        nbr_shards,
        online_cpus,
    ))
    .on_all_shards(enclose!((mesh) move || async move {

        // Join the full mesh to get this shard's sender and all receivers
        // The receivers are for receiving messages from other shards (synonymous with executors)
        let (sender, mut receivers) = mesh.join().await.unwrap();
        let shard_id = sender.peer_id();

        // Should be equivalent - just executor id is 1 based while peer id is 0 based
        let executor_id = glommio::executor().id();
        info!("Starting executor {executor_id} with shard id {shard_id}");

        // The same sender is used in multiple threads so we need to wrap it in a RefCell and Rc
        // This is safe because we are in a single threaded executor
        let sender = Rc::new(RefCell::new(sender));

        // Create the LRU cache for this shard
        // As an example only cache the last 3 computed Fibonacci numbers
        let fib_cache = Rc::new(RefCell::new(LruCache::<u64, u64>::new(
            NonZeroUsize::new(lru_cache_size).unwrap()
        )));

        // There is a receiver for each other shard that we must listen to
        // So we can spin up a thread for each of these, to allow concurrent processing from other shards
        // We still need the sender for THIS shard though as we may have to forward requests from clients to other shards
        // This is because we allow clients to have a persistent TCP connection to the server (pipelining)
        for (src_shard, stream) in receivers.streams() {
            let sender_clone = sender.clone();
            let cache_clone = fib_cache.clone();
            spawn_local(async move {
                while let Some(msg) = stream.recv().await {
                    debug!("Shard {shard_id} received message from {src_shard} with value: {msg}");

                    // Reconstruct TcpStream from raw fd
                    // Ensure to use a glommio TcpStream
                    // We can do this because the sender has forgotten the fd keeping it open
                    let mut tcp_stream = unsafe { glommio::net::TcpStream::from_raw_fd(msg.fd) };

                    // Other shard has accepted and already read the request data.
                    // So we can process it immediately and write the response back
                    let response = process_synchronously_on_shard(shard_id, msg.value, &cache_clone);
                    write_to_tcp_stream(response, &mut tcp_stream).await;

                    // Continue on the tcp connection to read more data from the client
                    // Note that we might still have to forward it on to the right shard again
                    // We need to clone sender again here as inside is a spawn local
                    process_tcp_stream(shard_id, nbr_shards, tcp_stream, sender_clone.clone(), cache_clone.clone()).await;
                }
            }).detach();
        }

        // Now that we have setup our listeners for other shards, we can start accepting TCP connections from clients
        // We will accept connections on port 10000 on localhost
        let listener = TcpListener::bind("0.0.0.0:10000").unwrap();
        info!("Shard {shard_id} listening on {}", listener.local_addr().unwrap());

        // An infinite loop to accept incoming TCP connections
        // Essentially a fire + forget model where each connection is handled in its own task
        loop {
            match listener.accept().await {
                Ok(tcp_stream) => {
                    match tcp_stream.peer_addr() {
                        Ok(addr) => debug!("Shard {shard_id} accepted connection from {addr}"),
                        Err(_) => error!("Shard {shard_id} accepted connection from unknown address"),
                    }
                    process_tcp_stream(shard_id, nbr_shards, tcp_stream, sender.clone(), fib_cache.clone()).await;
                }
                Err(e) => {
                    error!("Shard {shard_id} failed to accept connection: {e}");
                    // Continue listening for other connections instead of crashing
                    continue;
                }
            }
        }

    }))
    .unwrap()
    .join_all();
}


async fn process_tcp_stream(
    shard_id: usize, 
    nbr_shards: usize, 
    mut tcp_stream: glommio::net::TcpStream, 
    sender: Rc<RefCell<Senders<Msg>>>,
    fib_cache: Rc<RefCell<LruCache<u64, u64>>>) {
    // It's critical to spawn local here as this allows accepting of new connections
    // It also allows processing messages sent from other shards 
    // Both of these can happen with spawn local while still listening to an open TCP connection    
    spawn_local(async move {
        loop {
            let number_input = match read_from_tcp_stream(shard_id, &mut tcp_stream).await {
                Some(num) => num,
                None => return,
            };

            // Determine which shard to route this request to
            // If we are already on the correct shard, we can process it directly without an additional spawn local
            let idx = (number_input as usize) % nbr_shards;

            if idx != shard_id {
                // Leave the TCP connection from the client open
                // This effectively transfers the connection to another shard
                let fd = tcp_stream.into_raw_fd();
                let msg = Msg { value: number_input, fd};
                
                // Try to send the message to the target shard
                match sender.borrow().try_send_to(idx, msg) {
                    Ok(()) => {
                        break;
                    }
                    Err(_) => {
                        // Channel is full or other error occurred
                        error!("Shard {shard_id} failed to forward message to shard {idx}: channel full or unavailable. Closing connection.");
                        
                        // Reconstruct the stream to properly close it
                        let mut tcp_stream = unsafe { glommio::net::TcpStream::from_raw_fd(fd) };
                        
                        // Write an error response to the client before closing
                        let error_response = "Server is overwhelmed. Please try again later.\n";
                        if let Err(e) = tcp_stream.write_all(error_response.as_bytes()).await {
                            error!("Failed to write error response: {e}");
                        }
                        
                        // The tcp_stream will be properly dropped here, closing the connection
                        return;
                    }
                }
            } else {
                let response = process_synchronously_on_shard(shard_id, number_input, &fib_cache);
                write_to_tcp_stream(response, &mut tcp_stream).await;
            }
        }
    }).detach();
}

/// A CPU intensive function that computes the Fibonacci number
/// This is a synchronous function that blocks the current thread
/// It is called on the shard that is responsible for the given value
fn process_synchronously_on_shard(
    src_shard: usize, 
    value: u64,
    fib_cache: &Rc<RefCell<LruCache<u64, u64>>>) -> String {

    // Check cache first
    if let Some(&cached_result) = fib_cache.borrow().peek(&value) {
        let response = format!("Reply from shard {src_shard}: received value {value} and cached fibonacci {cached_result}\n");
        return response;
    }
    
    // Compute if not in cache
    let fib = compute_fibonacci(value);
    
    // Store in cache
    fib_cache.borrow_mut().put(value, fib);
    
    let response = format!("Reply from shard {src_shard}: received value {value} and fibonacci {fib}\n");
    response
}

/// Write the response back to the TCP stream
/// Note we can be async here as we have already completed our syncronous 'work'
async fn write_to_tcp_stream(response: String, tcp_stream: &mut glommio::net::TcpStream) {
    if let Err(e) = tcp_stream.write_all(response.as_bytes()).await {
        error!("Failed to write to TCP stream: {e}");
    }
}

/// Read all available bytes on the TCP stream, up to 1024 bytes
/// Return None if the connection is closed or no data is read
/// Otherwise return the parsed u64 value
/// Note we can be async here as we are just reading from the TCP stream and other 
/// tasks can proceed while we wait for data
async fn read_from_tcp_stream(shard_id: usize, tcp_stream: &mut glommio::net::TcpStream) -> Option<u64> {
    let mut tcp_buffer = vec![0u8; 1024];
    let n = match tcp_stream.read(&mut tcp_buffer).await {
        Ok(bytes_read) => bytes_read,
        Err(e) => {
            error!("Shard {shard_id} failed to read from TCP stream: {e}");
            return None;
        }
    };

    let string_input = String::from_utf8_lossy(&tcp_buffer[..n]);

    let string_input_trimmed = string_input.trim_end();
    if string_input_trimmed.is_empty() {
        return None;
    }

    debug!("Shard {shard_id} received data: {string_input_trimmed}");
    
    match string_input_trimmed.parse::<u64>() {
        Ok(number_input) => {
            debug!("Shard {shard_id} parsed number: {number_input}");
            Some(number_input)
        }
        Err(_) => {
            error!("Shard {shard_id} failed to parse '{string_input_trimmed}' as u64");
            None
        }
    }
}