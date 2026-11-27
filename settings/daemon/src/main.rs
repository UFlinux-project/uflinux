mod audio;
mod bluetooth;
mod capture;
mod devices;
mod display;
mod network;
mod printing;
mod rdp;
mod session;
mod vpn;
fn main() {
    if let Err(error) = session::run() {
        eprintln!("uflinux-settingsd: {error}");
        std::process::exit(1);
    }
}
