// Thin leaf: one small file over the egui/wgpu/winit stack — the
// "edit one widget" shape.
fn ui(ctx: &egui::Context) {
    egui::Window::new("demo").show(ctx, |ui| {
        ui.label("iteration probe");
        if ui.button("bump").clicked() {
            eprintln!("clicked");
        }
    });
}

fn main() {
    // Touch the graphics stack enough to pull real code into the link.
    let _attrs = wgpu::RequestAdapterOptions::default();
    let _ev: Option<winit::event::Event<()>> = None;
    let _f: fn(&egui::Context) = ui;
    let n = egui::Color32::from_rgb(54, 0, 2);
    println!("leaf ok {n:?}");
}
