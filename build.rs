fn main() {
    println!("cargo:rustc-link-lib=user32");
    println!("cargo:rustc-link-lib=advapi32");
    println!("cargo:rustc-link-lib=comdlg32");
    // 静态链接 OpenCV 往往还需要以下两个常见系统动态库，建议一并加上
    println!("cargo:rustc-link-lib=gdi32");
    println!("cargo:rustc-link-lib=shell32");
}