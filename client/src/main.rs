use std::io::{Write, Read};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use rand::Rng;

struct BenchmarkStats {
    total_requests: u64,
    total_duration: Duration,
    start_time: Instant,
}

impl BenchmarkStats {
    fn new() -> Self {
        Self {
            total_requests: 0,
            total_duration: Duration::new(0, 0),
            start_time: Instant::now(),
        }
    }

    fn add_request(&mut self, duration: Duration) {
        self.total_requests += 1;
        self.total_duration += duration;
    }

    fn requests_per_second(&self) -> f64 {
        let elapsed = self.start_time.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            self.total_requests as f64 / elapsed
        } else {
            0.0
        }
    }

    fn average_response_time_ms(&self) -> f64 {
        if self.total_requests > 0 {
            self.total_duration.as_millis() as f64 / self.total_requests as f64
        } else {
            0.0
        }
    }
}

fn main() {
    println!("Fibonacci TCP Client Benchmark");
    println!("==============================");

    // Updated configuration for higher throughput
    let server_addr = "127.0.0.1:10000";
    let num_connections = 50; // Increased concurrent connections
    let test_duration = Duration::from_secs(30); // Run for 30 seconds
    
    println!("Configuration:");
    println!("- Server: {}", server_addr);
    println!("- Concurrent connections: {}", num_connections);
    println!("- Test duration: {:?}", test_duration);
    println!("- Will send requests as fast as possible");
    println!();

    // Shared statistics
    let stats = Arc::new(Mutex::new(BenchmarkStats::new()));
    let mut handles = Vec::new();

    // Start benchmark
    let benchmark_start = Instant::now();
    println!("Starting benchmark...");

    // Spawn worker threads
    for connection_id in 0..num_connections {
        let stats_clone = Arc::clone(&stats);
        let server_addr = server_addr.to_string();
        
        let handle = thread::spawn(move || {
            run_client_connection(connection_id, &server_addr, stats_clone, test_duration);
        });
        
        handles.push(handle);
    }

    // Print periodic statistics
    let stats_clone = Arc::clone(&stats);
    let stats_handle = thread::spawn(move || {
        let mut last_requests = 0;
        let mut last_time = Instant::now();
        
        loop {
            thread::sleep(Duration::from_secs(2));
            
            let stats_guard = stats_clone.lock().unwrap();
            let current_requests = stats_guard.total_requests;
            let overall_rps = stats_guard.requests_per_second();
            let avg_response_time = stats_guard.average_response_time_ms();
            
            let requests_in_period = current_requests - last_requests;
            let time_elapsed = last_time.elapsed().as_secs_f64();
            let period_rps = requests_in_period as f64 / time_elapsed;
            
            last_requests = current_requests;
            last_time = Instant::now();
            
            println!("Stats: {} total requests | Overall: {:.1} RPS | Last 2s: {:.1} RPS | Avg latency: {:.1}ms", 
                    current_requests, overall_rps, period_rps, avg_response_time);
            
            if stats_guard.start_time.elapsed() >= test_duration {
                break;
            }
        }
    });

    // Wait for all connections to finish or timeout
    for handle in handles {
        handle.join().unwrap();
    }

    // Stop stats thread
    stats_handle.join().unwrap();

    // Final statistics
    let final_stats = stats.lock().unwrap();
    let total_time = benchmark_start.elapsed();
    
    println!("\n=== FINAL BENCHMARK RESULTS ===");
    println!("Total time: {:.2}s", total_time.as_secs_f64());
    println!("Total requests: {}", final_stats.total_requests);
    println!("Requests per second: {:.1}", final_stats.requests_per_second());
    println!("Average response time: {:.2}ms", final_stats.average_response_time_ms());
}

fn run_client_connection(
    connection_id: usize, 
    server_addr: &str, 
    stats: Arc<Mutex<BenchmarkStats>>,
    max_duration: Duration,
) {
    let mut stream = match TcpStream::connect(server_addr) {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("Connection {}: Failed to connect: {}", connection_id, e);
            return;
        }
    };

    // Set timeouts but make them longer
    if let Err(e) = stream.set_read_timeout(Some(Duration::from_secs(5))) {
        eprintln!("Connection {}: Failed to set read timeout: {}", connection_id, e);
        return;
    }

    if let Err(e) = stream.set_write_timeout(Some(Duration::from_secs(5))) {
        eprintln!("Connection {}: Failed to set write timeout: {}", connection_id, e);
        return;
    }

    // Disable Nagle's algorithm for lower latency
    if let Err(e) = stream.set_nodelay(true) {
        eprintln!("Connection {}: Failed to set nodelay: {}", connection_id, e);
    }

    let mut rng = rand::thread_rng();
    let start_time = Instant::now();
    let mut request_count = 0u64;
    let mut buffer = vec![0u8; 4096]; // Larger buffer

    println!("Connection {}: Connected, starting requests", connection_id);

    loop {
        // Check if we should stop
        if start_time.elapsed() >= max_duration {
            break;
        }

        // Generate random Fibonacci number between 1 and 1000
        let fib_input: u64 = rng.gen_range(1..=1000);
        
        let request_start = Instant::now();
        
        // Send request
        let request = format!("{}\n", fib_input);
        if let Err(e) = stream.write_all(request.as_bytes()) {
            eprintln!("Connection {}: Failed to send request: {}", connection_id, e);
            break;
        }
        
        // Read response
        match stream.read(&mut buffer) {
            Ok(bytes_read) => {
                if bytes_read == 0 {
                    println!("Connection {}: Server closed connection after {} requests", connection_id, request_count);
                    break;
                }
                
                let request_duration = request_start.elapsed();
                
                // Update statistics (minimize lock time)
                {
                    let mut stats_guard = stats.lock().unwrap();
                    stats_guard.add_request(request_duration);
                }
                
                request_count += 1;
                
                // Print occasional responses for verification
                if request_count % 1000 == 0 {
                    let response = String::from_utf8_lossy(&buffer[..bytes_read.min(100)]);
                    println!("Connection {}: Request {}, Input: {}, Response preview: {}", 
                            connection_id, request_count, fib_input, response.trim());
                }
            }
            Err(e) => {
                eprintln!("Connection {}: Failed to read response: {}", connection_id, e);
                break;
            }
        }
    }

    println!("Connection {}: Completed {} requests in {:.2}s", 
             connection_id, request_count, start_time.elapsed().as_secs_f64());
}