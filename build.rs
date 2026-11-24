fn main() {
    embed_resource::compile("res/files.rc", embed_resource::NONE).manifest_optional().unwrap();
    println!("cargo:rerun-if-changed=res/files.rc");
    println!("cargo:rerun-if-changed=res/files.ico");
    println!("cargo:rerun-if-changed=ui");
}
