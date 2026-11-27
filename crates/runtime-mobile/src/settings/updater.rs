//! The Discord update page: what the update brings and what Sunbird checked,
//! then the overall progress over a list of steps that grows as each starts,
//! each with its own bar as on the site's patcher, and the failure or the
//! hand-off to Android's installer.

use phoenix_ui_mobile::{
   Prop,
   ViewKind,
   prop,
};
use phoenix_wire::Value;

use crate::{
   discord_update::{
      self,
      Build,
      Progress,
      State,
   },
   settings::{
      ICON_RELOAD,
      Press,
      cards::{
         BLURPLE,
         GREEN,
         badge,
         bar,
         button,
         card,
         column,
         fixed,
         grow,
         heading,
         label,
         line,
         node,
         row,
      },
      theme::{
         Color,
         template_color,
      },
      tree::Tree,
   },
};

pub fn sections() -> Vec<Tree> {
   let installed = discord_update::installed_name();
   let card = match discord_update::state() {
      State::Current => {
         card(vec![heading(
            badge(GREEN, 52.0, "\u{2713}"),
            "Discord is up to date",
            &installed,
            None,
         )])
      },
      State::Available(build) => offer(&build, &installed),
      State::Running(build, progress) => running(&build, &progress, &installed),
      State::Failed {
         build,
         error,
         progress,
      } => {
         let mut kids = vec![
            heading(
               badge(Color::Error.argb(), 52.0, "!"),
               "Update failed",
               &format!("Discord {}", build.version_name),
               None,
            ),
            line(&error, Color::Muted.argb()),
         ];
         kids.extend(progress.map(|progress| steps(&progress, true)));
         kids.push(button("Try again", Press::StartDiscordUpdate));
         card(kids)
      },
      State::Handed(build) => {
         card(vec![
            heading(
               badge(GREEN, 52.0, "\u{2713}"),
               "Ready to install",
               &format!("Discord {}", build.version_name),
               None,
            ),
            line(
               "Android's installer has the update. Tap Update there, and Discord restarts on the \
                new version.",
               Color::Muted.argb(),
            ),
            button("Open the installer", Press::StartDiscordUpdate),
         ])
      },
   };
   vec![card]
}

fn offer(build: &Build, installed: &str) -> Tree {
   let size = discord_update::download_size(build) >> 20;
   let subtitle = if installed.is_empty() {
      "A newer Discord is ready".to_owned()
   } else {
      format!("You have {installed}")
   };
   card(vec![
      heading(
         badge(BLURPLE, 52.0, ICON_RELOAD),
         &format!("Discord {}", build.version_name),
         &subtitle,
         None,
      ),
      row(8.0, vec![
         chip(&format!("{size} MB")),
         chip("Signed by Discord"),
         chip("Login kept"),
      ]),
      column(10.0, vec![
         label("What Sunbird checked", 13.0, 700, Color::Muted.argb()),
         check("Discord's own signature on every file"),
         check("Sunbird's patches and plugin hooks fit this build"),
         check("Each download matches before it installs"),
      ]),
      button(
         &format!("Download and install \u{00B7} {size} MB"),
         Press::StartDiscordUpdate,
      ),
   ])
}

fn running(build: &Build, progress: &Progress, installed: &str) -> Tree {
   let versions = if installed.is_empty() {
      build.version_name.clone()
   } else {
      format!("{installed}  \u{2192}  {}", build.version_name)
   };
   let pace = progress.pace().map_or_else(String::new, |(speed, left)| {
      let left = if left < 60 {
         format!("{left} s left")
      } else {
         format!("{} min left", left.div_ceil(60))
      };
      format!("{:.1} MB/s \u{00B7} {left}", speed / f64::from(1 << 20))
   });
   card(vec![
      heading(
         badge(BLURPLE, 52.0, ICON_RELOAD),
         "Updating Discord",
         &versions,
         Some(label(
            &format!("{}%", progress.percent()),
            22.0,
            700,
            BLURPLE,
         )),
      ),
      column(8.0, vec![
         bar(progress.fraction(), 8.0, BLURPLE),
         row(8.0, vec![
            grow(label(
               &format!(
                  "{} of {}",
                  megabytes(progress.done),
                  megabytes(progress.total)
               ),
               13.0,
               400,
               Color::Muted.argb(),
            )),
            label(&pace, 13.0, 400, Color::Muted.argb()),
         ]),
      ]),
      steps(progress, false),
      line(
         "Keep Discord open, the installer opens when it is ready.",
         Color::Muted.argb(),
      ),
   ])
}

/// The steps begun so far, each with its own bar and state: done in green,
/// the current one with its bytes or at work, or failed in red.
fn steps(progress: &Progress, failed: bool) -> Tree {
   let rows = progress
      .steps
      .iter()
      .enumerate()
      .take(progress.at + 1)
      .map(|(index, step)| {
         let (fraction, state, color) = if index < progress.at {
            let state = step.size.map_or_else(|| "done".to_owned(), tenths);
            (1.0, state, GREEN)
         } else if failed {
            (
               fraction_of(progress, index),
               "failed".to_owned(),
               Color::Error.argb(),
            )
         } else if let Some(size) = step.size {
            let fetched = progress.fetched(index);
            (
               fraction_of(progress, index),
               format!("{} / {}", tenths(fetched), tenths(size)),
               BLURPLE,
            )
         } else {
            (0.5, "working\u{2026}".to_owned(), BLURPLE)
         };
         let state_color = if color == BLURPLE {
            Color::Muted.argb()
         } else {
            color
         };
         column(6.0, vec![
            row(12.0, vec![
               grow(label(&step.label, 15.0, 600, Color::Text.argb())),
               label(&state, 13.0, 400, state_color),
            ]),
            bar(fraction, 4.0, color),
         ])
      })
      .collect();
   column(14.0, rows)
}

/// How much of download step `index` has arrived, from 0 to 1.
fn fraction_of(progress: &Progress, index: usize) -> f64 {
   let size = progress
      .steps
      .get(index)
      .and_then(|step| step.size)
      .unwrap_or(0);
   if size == 0 {
      return 0.0;
   }
   #[expect(
      clippy::cast_precision_loss,
      reason = "a bar's width needs no byte precision"
   )]
   let fraction = progress.fetched(index) as f64 / size as f64;
   fraction.clamp(0.0, 1.0)
}

/// `bytes` in MB to a tenth, as the site shows them.
fn tenths(bytes: usize) -> String {
   #[expect(
      clippy::cast_precision_loss,
      reason = "megabytes to a tenth need no byte precision"
   )]
   let megabytes = bytes as f64 / f64::from(1 << 20);
   format!("{megabytes:.1} MB")
}

/// `bytes` in MB, or KB under one.
fn megabytes(bytes: usize) -> String {
   if bytes < 1 << 20 {
      format!("{} KB", bytes.div_ceil(1 << 10))
   } else {
      format!("{} MB", bytes >> 20)
   }
}

fn check(text_value: &str) -> Tree {
   row(10.0, vec![
      fixed(label("\u{2713}", 14.0, 700, GREEN), 16.0),
      grow(label(text_value, 14.0, 400, Color::Text.argb())),
   ])
}

/// A small tinted tag.
fn chip(text_value: &str) -> Tree {
   node(
      ViewKind::View,
      vec![
         (Prop::PADDING, Value::Rect([10.0, 4.0, 10.0, 4.0])),
         (Prop::RADIUS, Value::F64(12.0)),
         (prop!("background"), Value::I64(template_color(0x1F97_979F))),
      ],
      vec![label(text_value, 12.0, 600, Color::Text.argb())],
   )
}
