fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=../../assets/agentdictate.ico");
        println!("cargo:rerun-if-changed=windows.rc");
        embed_resource::compile("windows.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("embed Windows icon");
    }
}
