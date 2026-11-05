mod backend;
mod client;
mod proxy;
mod service;

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn main() -> Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("proxy") => proxy::run(&args[1..]),
        Some("backend") | Some("client") => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?;
            runtime.block_on(async {
                if args[0] == "backend" {
                    backend::run(&args[1..]).await
                } else {
                    client::run(&args[1..]).await
                }
            })
        }
        _ => Err("usage: rill-grpc-acceptance backend|proxy|client [arguments]".into()),
    }
}
