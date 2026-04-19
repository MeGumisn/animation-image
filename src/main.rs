mod image_animation;
mod dxgi_capture_rs;
mod capture_settings;

#[show_image::main]
fn main() {
    image_animation::animation_test().unwrap();
}
