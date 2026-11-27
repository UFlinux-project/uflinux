mod boot;
mod operations;
mod power;
mod printing;
mod routes;
fn main() {
    if let Err(error) = operations::run() {
        eprintln!("uflinux-settings-system: {error}");
        std::process::exit(1);
    }
}
