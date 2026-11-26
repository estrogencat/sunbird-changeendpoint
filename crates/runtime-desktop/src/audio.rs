use phoenix_hook::image::Image;
use phoenix_sigs::desktop;

const IDENTITY: [f32; 5] = [0.0, 0.0, 1.0, 0.0, 0.0];

pub fn install(image: &Image<'static>) {
   let table = match desktop::high_pass_coefficients(image) {
      Ok(table) => table,
      Err(err) => {
         log::warn!("audio: {err}, the capture high-pass filter stays on");
         return;
      },
   };
   let bytes: Vec<u8> = IDENTITY
      .iter()
      .cycle()
      .take(desktop::HIGH_PASS_COEFFICIENTS.len())
      .flat_map(|value| value.to_le_bytes())
      .collect();
   match unsafe { image.write_code(table, &bytes) } {
      Ok(()) => log::info!("audio: capture high-pass filter passes audio through"),
      Err(err) => log::warn!("audio: could not flatten the high-pass filter: {err}"),
   }
}
