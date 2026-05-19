use axum::{routing::get, Router};
use std::{env, net::SocketAddr, time::Duration};
use tokio::signal;

#[tokio::main]
async fn main() {
    // 1. Simulate a slow startup sequence
    let startup_delay_secs = env::var("STARTUP_DELAY")
        .unwrap_or_else(|_| "10".to_string()) // Default to 10 seconds
        .parse()
        .unwrap_or(10);
        
    println!("Initializing heavy resources... (Sleeping for {} seconds)", startup_delay_secs);
    tokio::time::sleep(Duration::from_secs(startup_delay_secs)).await;
    println!("Initialization complete.");

    // 2. Identify the environment for the polling script
    let app_version = env::var("APP_VERSION").unwrap_or_else(|_| "Blue (Default)".to_string());
    
    // Create a clone of the version string to move into the async closure
    let route_version = app_version.clone();

    // 3. Define the routes
    let app = Router::new()
        // The main endpoint that your test will poll to verify the active version
        .route("/", get(move || async move { 
            format!("Hello from version: {}\n", route_version) 
        }))
        // Load balancers need a fast health check endpoint
        .route("/health", get(|| async { "OK\n" }));

    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()
        .unwrap_or(8080);
        
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    
    // 4. Bind and Serve with Graceful Shutdown
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    println!("[{}] listening on {}", app_version, addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

/// Helper function to handle termination signals for zero-downtime
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    // Wait for either Ctrl+C or a termination signal (like from Kubernetes/Docker)
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    println!("Termination signal received. Shutting down gracefully...");
    
    // Optional: Add a small delay here if your infrastructure needs a moment 
    // to remove this instance from the load balancer pool before we stop accepting requests.
    tokio::time::sleep(Duration::from_secs(2)).await;
}