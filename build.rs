fn main() {
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        winresource::WindowsResource::new().set_icon("assets/icon.ico").compile().unwrap();
    }
}
