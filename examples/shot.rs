//! Проверка захвата экрана в отрыве от остального.
//! cargo run --release --example shot

fn main() -> anyhow::Result<()> {
    let t = std::time::Instant::now();
    let shot = ghost::screen::capture(1568, ghost::screen::Target::Window)?;
    println!(
        "{} · {}×{}, {} КБ, за {} мс",
        shot.source,
        shot.width,
        shot.height,
        shot.png.len() / 1024,
        t.elapsed().as_millis()
    );
    std::fs::write("dumps/xcap.png", &shot.png)?;
    println!("записан в dumps/xcap.png");
    Ok(())
}
