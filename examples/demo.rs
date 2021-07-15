fn main() {
    let test_crate_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test_crate")
        .join("Cargo.toml");
    println!("Begin watching for changes to {:?}", test_crate_path);
    let watch = hotlib::watch(&test_crate_path).unwrap();
    loop {
        let pkg = watch.next().unwrap();
        let build = pkg.build().unwrap();
        unsafe {
            let lib = build.load().unwrap();
            let foo: libloading::Symbol<fn(i32, i32) -> i32> = lib.get(b"foo").unwrap();
            let res = foo(6, 7);
            println!("{}", res);
        }
        println!("Awaiting next change...");
    }
}
