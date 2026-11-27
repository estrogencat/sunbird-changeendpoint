//! Cards, rows, text and buttons built from plain views, for pages Phoenix
//! has no captured layout for. Phoenix's own groups shrink children they do
//! not know, so these stretch themselves.

use std::sync::atomic::{
   AtomicU64,
   Ordering,
};

use phoenix_ui_mobile::{
   Prop,
   Props,
   ViewKind,
   motion::Spring,
   prop,
   templates,
};
use phoenix_wire::Value;

use crate::settings::{
   Press,
   theme::{
      Color,
      template_color,
   },
   tree::{
      Animate,
      Tree,
      text,
   },
};

pub const BLURPLE: i64 = 0xFF58_65F2;
pub const GREEN: i64 = 0xFF23_A55A;
pub const WHITE: i64 = 0xFFFF_FFFF;
/// The settings rows' translucent surface and the track under a bar, as the
/// captured dark layouts have them; [`template_color`] maps them for light.
const SURFACE: i64 = 0x1497_979F;
const TRACK: i64 = 0x1F97_979F;
/// Damped past critical (2 * sqrt(200) is about 28.3), so a bar eases into
/// each new width without overshooting and never moves back.
const EASE: Spring = Spring {
   mass:      1.0,
   stiffness: 200.0,
   damping:   30.0,
};
/// A bar track's width in dp, as its `layout` event last gave it. Every bar
/// spans its card, so one width serves them all.
static BAR_WIDTH: AtomicU64 = AtomicU64::new(0);

/// Records a bar track's width, and whether it is the first, which the bars
/// drawn before it need a refresh for. Later ones apply on the next render,
/// so bars of slightly different widths cannot refresh each other forever.
pub fn measured(width: f64) -> bool {
   width > 0.0 && BAR_WIDTH.swap(width.to_bits(), Ordering::Relaxed) == 0
}

pub fn card(kids: Vec<Tree>) -> Tree {
   node(
      ViewKind::View,
      vec![
         (prop!("direction"), Value::Str("column".to_owned())),
         (prop!("width"), Value::Str("fill".to_owned())),
         (prop!("align"), Value::Str("stretch".to_owned())),
         (prop!("background"), Value::I64(template_color(SURFACE))),
         (Prop::RADIUS, Value::F64(16.0)),
         (Prop::PADDING, Value::Rect([18.0, 18.0, 18.0, 18.0])),
         (Prop::GAP, Value::F64(16.0)),
      ],
      kids,
   )
}

pub fn row(gap: f64, kids: Vec<Tree>) -> Tree {
   node(
      ViewKind::View,
      vec![
         (prop!("direction"), Value::Str("row".to_owned())),
         (prop!("width"), Value::Str("fill".to_owned())),
         (prop!("align"), Value::Str("center".to_owned())),
         (Prop::GAP, Value::F64(gap)),
      ],
      kids,
   )
}

pub fn column(gap: f64, kids: Vec<Tree>) -> Tree {
   node(
      ViewKind::View,
      vec![
         (prop!("direction"), Value::Str("column".to_owned())),
         (prop!("width"), Value::Str("fill".to_owned())),
         (prop!("align"), Value::Str("stretch".to_owned())),
         (Prop::GAP, Value::F64(gap)),
      ],
      kids,
   )
}

/// A thin line between a card's items.
pub fn divider() -> Tree {
   node(
      ViewKind::View,
      vec![
         (prop!("width"), Value::Str("fill".to_owned())),
         (Prop::HEIGHT, Value::F64(1.0)),
         (prop!("background"), Value::I64(template_color(TRACK))),
      ],
      Vec::new(),
   )
}

/// A paragraph across its parent.
pub fn line(value: &str, color: i64) -> Tree {
   let mut node = label(value, 14.0, 400, color);
   node.set(prop!("width"), Some(Value::Str("fill".to_owned())));
   node
}

pub fn label(value: &str, size: f64, weight: i32, color: i64) -> Tree {
   node(
      ViewKind::Text,
      vec![
         (Prop::TEXT, text(value)),
         (Prop::COLOR, Value::I64(color)),
         (Prop::FONT_SIZE, Value::F64(size)),
         (prop!("fontWeight"), Value::I32(weight)),
         (Prop::LINE_HEIGHT, Value::F64(size + 6.0)),
      ],
      Vec::new(),
   )
}

/// A round `color` badge holding `icon` (a `res://` image) or a glyph.
pub fn badge(color: i64, size: f64, icon: &str) -> Tree {
   let inner = if icon.starts_with("res://") {
      node(
         ViewKind::Image,
         vec![
            (Prop::URI, text(icon)),
            (prop!("width"), Value::F64(size * 0.55)),
            (Prop::HEIGHT, Value::F64(size * 0.55)),
            (Prop::TINT, Value::I64(WHITE)),
            (prop!("scale"), Value::Str("fit".to_owned())),
         ],
         Vec::new(),
      )
   } else {
      label(icon, size * 0.45, 700, WHITE)
   };
   node(
      ViewKind::View,
      vec![
         (prop!("width"), Value::F64(size)),
         (Prop::HEIGHT, Value::F64(size)),
         (Prop::RADIUS, Value::F64(size / 2.0)),
         (prop!("background"), Value::I64(color)),
         (prop!("align"), Value::Str("center".to_owned())),
         (prop!("justify"), Value::Str("center".to_owned())),
      ],
      vec![inner],
   )
}

/// A badge, a title and a subtitle, and what trails them.
pub fn heading(badge: Tree, title: &str, subtitle: &str, trailing: Option<Tree>) -> Tree {
   let mut kids = vec![
      badge,
      grow(column(2.0, vec![
         label(title, 17.0, 700, Color::Text.argb()),
         label(subtitle, 14.0, 400, Color::Muted.argb()),
      ])),
   ];
   kids.extend(trailing);
   row(14.0, kids)
}

/// A full-width button.
pub fn button(text_value: &str, press: Press) -> Tree {
   pressable(
      vec![
         (prop!("width"), Value::Str("fill".to_owned())),
         (Prop::HEIGHT, Value::F64(48.0)),
         (Prop::RADIUS, Value::F64(10.0)),
      ],
      label(text_value, 16.0, 700, WHITE),
      text_value,
      press,
   )
}

/// A small rounded button for the end of a row.
pub fn pill(text_value: &str, press: Press) -> Tree {
   pressable(
      vec![
         (Prop::HEIGHT, Value::F64(34.0)),
         (Prop::RADIUS, Value::F64(17.0)),
         (Prop::PADDING, Value::Rect([16.0, 0.0, 16.0, 0.0])),
      ],
      label(text_value, 14.0, 700, WHITE),
      text_value,
      press,
   )
}

fn pressable(mut props: Props, content: Tree, name: &str, press: Press) -> Tree {
   let on = templates()
      .button_row
      .props
      .iter()
      .find(|(key, _)| *key == Prop::ON)
      .map_or_else(
         || Value::Arr(vec![text("press")]),
         |(_, value)| value.clone(),
      );
   props.extend([
      (prop!("background"), Value::I64(BLURPLE)),
      (prop!("activeBackground"), Value::I64(BLURPLE & 0xCCFF_FFFF)),
      (prop!("align"), Value::Str("center".to_owned())),
      (prop!("justify"), Value::Str("center".to_owned())),
      (Prop::ROLE, text("button")),
      (Prop::ACCESSIBILITY_LABEL, text(name)),
      (Prop::ACCESSIBILITY_ID, text(&format!("phoenix-{name}"))),
      (Prop::ON, on),
   ]);
   let mut tree = node(ViewKind::View, props, vec![content]);
   tree.press = Some(press);
   tree
}

/// A bar filled `fraction` of the way in `color`. Once a track has reported
/// its width, the fill has one in dp and springs to each new width on the
/// host; until then it is two flex weights.
pub fn bar(fraction: f64, height: f64, color: i64) -> Tree {
   let fill_props = |width: (Prop, Value)| {
      vec![
         width,
         (Prop::HEIGHT, Value::F64(height)),
         (prop!("background"), Value::I64(color)),
         (Prop::RADIUS, Value::F64(height / 2.0)),
      ]
   };
   let track = f64::from_bits(BAR_WIDTH.load(Ordering::Relaxed));
   let kids = if track > 0.0 {
      let props = fill_props((prop!("width"), Value::F64(track * fraction)));
      let mut fill = node(ViewKind::View, props.clone(), Vec::new());
      fill.animate = Some(Animate {
         worklet: &templates().knob_worklet,
         other:   props,
         flipped: false,
         spring:  EASE,
      });
      vec![fill]
   } else {
      vec![
         node(
            ViewKind::View,
            fill_props((prop!("flex"), Value::F64(fraction))),
            Vec::new(),
         ),
         node(
            ViewKind::View,
            vec![(prop!("flex"), Value::F64(1.0 - fraction))],
            Vec::new(),
         ),
      ]
   };
   let mut bar = node(
      ViewKind::View,
      vec![
         (prop!("direction"), Value::Str("row".to_owned())),
         (prop!("width"), Value::Str("fill".to_owned())),
         (Prop::HEIGHT, Value::F64(height)),
         (Prop::RADIUS, Value::F64(height / 2.0)),
         (prop!("clip"), Value::Bool(true)),
         (prop!("background"), Value::I64(template_color(TRACK))),
         (Prop::ON, Value::Arr(vec![text("layout")])),
      ],
      kids,
   );
   bar.press = Some(Press::Measure);
   bar
}

pub fn grow(mut tree: Tree) -> Tree {
   tree.set(prop!("flex"), Some(Value::F64(1.0)));
   tree
}

pub fn fixed(mut tree: Tree, width: f64) -> Tree {
   tree.set(prop!("width"), Some(Value::F64(width)));
   tree
}

pub const fn node(kind: ViewKind, props: Props, kids: Vec<Tree>) -> Tree {
   Tree {
      id: 0,
      kind,
      props,
      kids,
      src: None,
      press: None,
      slot: None,
      animate: None,
   }
}
