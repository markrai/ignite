//! Render the sleep-screen Ignite mark with the exact runtime renderer
//! (`sleep_ui::render_sleep`) into an 8-bit grayscale PGM. Used to generate the Kindle Library
//! icon (`tools/kindle_launcher/assets/scribe-icon.png`) so it matches the screensaver.
//!
//! `cargo run -p scribe-app --example render_sleep_art -- WIDTH HEIGHT OUT.pgm`

use kindle_platform::FbGeometry;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [w, h, out] = args.as_slice() else {
        eprintln!("usage: render_sleep_art WIDTH HEIGHT OUT.pgm");
        std::process::exit(2);
    };
    let (w, h): (u32, u32) = (w.parse().expect("width"), h.parse().expect("height"));
    let geo = FbGeometry {
        xres: w,
        yres: h,
        xres_virtual: w,
        yres_virtual: h,
        xoffset: 0,
        yoffset: 0,
        bits_per_pixel: 8,
        line_length: w,
        smem_len: w * h,
    };
    let mut fb = vec![0u8; (w * h) as usize];
    scribe_app::sleep_ui::render_sleep(&mut fb, &geo).expect("render");
    let mut pgm = format!("P5\n{w} {h}\n255\n").into_bytes();
    pgm.extend_from_slice(&fb);
    std::fs::write(out, pgm).expect("write");
}
