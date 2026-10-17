//! Gives coffee-filter-core.exe the app's coffee icon and a description, so it's recognizable
//! in Task Manager, Explorer, and firewall prompts.

fn main() {
    println!("cargo:rerun-if-changed=../app/Assets/coffee.ico");

    let mut res = winresource::WindowsResource::new();
    res.set_icon("../app/Assets/coffee.ico");
    res.set("FileDescription", "Coffee Filter (filtering)");
    res.set("ProductName", "Coffee Filter");
    res.set("CompanyName", "Devin Green (Artistro08)");
    if let Err(e) = res.compile() {
        panic!("couldn't add the icon to coffee-filter-core.exe: {e}");
    }
}
