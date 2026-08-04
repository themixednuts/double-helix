use helix_ipc::VERSION_AND_GIT_HASH;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os();
    let _program = args.next();
    match (args.next().as_deref(), args.next()) {
        (None, None) => {
            helix_remote::server::run_stdio(VERSION_AND_GIT_HASH).await?;
            Ok(())
        }
        (Some(arg), None) if arg == "--version" => {
            println!("double-helix {VERSION_AND_GIT_HASH}");
            Ok(())
        }
        (Some(arg), None) if arg == "--identity" => {
            println!("{}", helix_remote::server_identity(VERSION_AND_GIT_HASH));
            Ok(())
        }
        _ => {
            eprintln!("dhx-server is an internal Double Helix component");
            std::process::exit(2);
        }
    }
}
