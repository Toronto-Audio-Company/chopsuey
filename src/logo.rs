//! The chopsuey logo (a takeout box), drawn from `assets/logo.svg`.
//!
//! vizia can't show SVGs, so this reads the small subset the logo uses:
//! `<path>` elements with absolute M / L / C / Z commands, `#rrggbb` or
//! `none` fill and stroke, and stroke-width (defaults from the `<svg>`
//! element). Edit the SVG and the plugin follows.

use nih_plug_vizia::vizia::prelude::*;
use nih_plug_vizia::vizia::vg;
use std::sync::OnceLock;

const SVG: &str = include_str!("../assets/logo.svg");

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cmd {
    Move(f32, f32),
    Line(f32, f32),
    Cubic(f32, f32, f32, f32, f32, f32),
    Close,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Shape {
    pub cmds: Vec<Cmd>,
    pub fill: Option<[u8; 3]>,
    pub stroke: Option<[u8; 3]>,
    pub stroke_width: f32,
}

/// The logo's view box size and shapes, in drawing order.
pub fn shapes() -> &'static (f32, Vec<Shape>) {
    static PARSED: OnceLock<(f32, Vec<Shape>)> = OnceLock::new();
    PARSED.get_or_init(|| parse(SVG).expect("assets/logo.svg is valid"))
}

/// Value of `name="..."` in an element's attributes.
fn attr<'a>(element: &'a str, name: &str) -> Option<&'a str> {
    let at = element.find(&format!(" {name}=\""))? + name.len() + 3;
    Some(&element[at..at + element[at..].find('"')?])
}

fn colour(value: &str) -> Result<Option<[u8; 3]>, String> {
    if value == "none" {
        return Ok(None);
    }
    let hex = value.strip_prefix('#').filter(|h| h.len() == 6).ok_or(format!("colour {value}"))?;
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| e.to_string());
    Ok(Some([byte(0)?, byte(2)?, byte(4)?]))
}

/// Numbers in path data: "6.8 9", "5.5 -1.5", "19.5-0.5".
fn numbers(data: &str) -> Result<Vec<f32>, String> {
    let mut out = Vec::new();
    let mut token = String::new();
    let flush = |token: &mut String, out: &mut Vec<f32>| -> Result<(), String> {
        if !token.is_empty() {
            out.push(token.parse().map_err(|_| format!("number {token}"))?);
            token.clear();
        }
        Ok(())
    };
    for c in data.chars() {
        match c {
            '-' => {
                flush(&mut token, &mut out)?;
                token.push(c);
            }
            '0'..='9' | '.' => token.push(c),
            _ => flush(&mut token, &mut out)?,
        }
    }
    flush(&mut token, &mut out)?;
    Ok(out)
}

fn path_data(d: &str) -> Result<Vec<Cmd>, String> {
    let mut cmds = Vec::new();
    let starts: Vec<usize> = d.match_indices(|c: char| c.is_ascii_alphabetic()).map(|(i, _)| i).collect();
    for (k, &i) in starts.iter().enumerate() {
        let end = starts.get(k + 1).copied().unwrap_or(d.len());
        let letter = &d[i..i + 1];
        let n = numbers(&d[i + 1..end])?;
        let each = match letter {
            "M" | "L" => 2,
            "C" => 6,
            "Z" | "z" => 0,
            _ => return Err(format!("path command {letter} (only absolute M L C Z)")),
        };
        if each == 0 {
            cmds.push(Cmd::Close);
            continue;
        }
        if n.is_empty() || n.len() % each != 0 {
            return Err(format!("{letter} needs numbers in {each}s"));
        }
        for (j, p) in n.chunks(each).enumerate() {
            cmds.push(match (letter, j) {
                ("M", 0) => Cmd::Move(p[0], p[1]),
                ("C", _) => Cmd::Cubic(p[0], p[1], p[2], p[3], p[4], p[5]),
                _ => Cmd::Line(p[0], p[1]), // extra M pairs are lines, as in SVG
            });
        }
    }
    Ok(cmds)
}

pub fn parse(svg: &str) -> Result<(f32, Vec<Shape>), String> {
    // Comments out, so they can mention elements.
    let mut svg = svg.to_string();
    while let Some(start) = svg.find("<!--") {
        let end = svg[start..].find("-->").ok_or("unclosed comment")? + start + 3;
        svg.replace_range(start..end, "");
    }
    let svg = svg.as_str();
    let root = &svg[svg.find("<svg").ok_or("no <svg>")?..];
    let root = &root[..root.find('>').ok_or("unclosed <svg>")?];
    let view = numbers(attr(root, "viewBox").ok_or("no viewBox")?)?;
    let size = *view.get(2).ok_or("viewBox needs 4 numbers")?;
    let fill = colour(attr(root, "fill").unwrap_or("#000000"))?;
    let stroke = colour(attr(root, "stroke").unwrap_or("none"))?;
    let width: f32 = attr(root, "stroke-width").map_or(Ok(1.0), |w| w.parse().map_err(|_| "stroke-width"))?;
    let mut shapes = Vec::new();
    for element in svg.split("<path").skip(1) {
        let element = &element[..element.find("/>").ok_or("unclosed <path>")?];
        shapes.push(Shape {
            cmds: path_data(attr(element, "d").ok_or("<path> without d")?)?,
            fill: attr(element, "fill").map_or(Ok(fill), colour)?,
            stroke: attr(element, "stroke").map_or(Ok(stroke), colour)?,
            stroke_width: attr(element, "stroke-width").map_or(Ok(width), |w| w.parse().map_err(|_| "stroke-width"))?,
        });
    }
    Ok((size, shapes))
}

/// Draws the logo to fill its bounds (square).
pub struct Logo;

impl Logo {
    pub fn new(cx: &mut Context) -> Handle<'_, Self> {
        Self.build(cx, |_| {}).hoverable(false)
    }
}

impl View for Logo {
    fn element(&self) -> Option<&'static str> {
        Some("logo")
    }

    fn draw(&self, cx: &mut DrawContext, canvas: &mut Canvas) {
        let b = cx.bounds();
        let (size, shapes) = shapes();
        let scale = b.w.min(b.h) / size;
        let (x0, y0) = (b.x + (b.w - size * scale) / 2.0, b.y + (b.h - size * scale) / 2.0);
        let at = |x: f32, y: f32| (x0 + x * scale, y0 + y * scale);
        for shape in shapes {
            let mut path = vg::Path::new();
            for cmd in &shape.cmds {
                match *cmd {
                    Cmd::Move(x, y) => {
                        let (x, y) = at(x, y);
                        path.move_to(x, y);
                    }
                    Cmd::Line(x, y) => {
                        let (x, y) = at(x, y);
                        path.line_to(x, y);
                    }
                    Cmd::Cubic(a, b, c, d, e, f) => {
                        let ((a, b), (c, d), (e, f)) = (at(a, b), at(c, d), at(e, f));
                        path.bezier_to(a, b, c, d, e, f);
                    }
                    Cmd::Close => path.close(),
                }
            }
            if let Some([r, g, b]) = shape.fill {
                canvas.fill_path(&path, &vg::Paint::color(vg::Color::rgb(r, g, b)));
            }
            if let Some([r, g, b]) = shape.stroke {
                let mut paint = vg::Paint::color(vg::Color::rgb(r, g, b));
                paint.set_line_width(shape.stroke_width * scale);
                paint.set_line_cap(vg::LineCap::Round);
                paint.set_line_join(vg::LineJoin::Round);
                canvas.stroke_path(&path, &paint);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_svg_parses() {
        let (size, shapes) = shapes();
        assert_eq!(*size, 24.0);
        assert_eq!(shapes.len(), 6);
        // The handle: a stroked curve with a negative coordinate.
        assert_eq!(shapes[0].cmds, vec![Cmd::Move(6.8, 9.0), Cmd::Cubic(5.5, -1.5, 19.5, -0.5, 17.2, 9.5)]);
        assert_eq!((shapes[0].fill, shapes[0].stroke, shapes[0].stroke_width), (None, Some([11, 11, 11]), 1.1));
        // A face: filled and outlined with the defaults.
        assert_eq!(shapes[1].fill, Some([0xc9, 0xc9, 0xc9]));
        assert_eq!(shapes[1].stroke_width, 1.5);
        assert_eq!(shapes[1].cmds.last(), Some(&Cmd::Close));
    }

    #[test]
    fn unsupported_paths_are_errors() {
        let svg = |d: &str| format!(r#"<svg viewBox="0 0 24 24"><path d="{d}"/></svg>"#);
        assert!(parse(&svg("M1 2L3 4Z")).is_ok());
        assert!(parse(&svg("m1 2l3 4")).is_err(), "relative commands");
        assert!(parse(&svg("M1 2A1 1 0 0 1 3 4")).is_err(), "arcs");
        assert!(parse(&svg("M1 2L3")).is_err(), "odd numbers");
        let commented = r#"<svg viewBox="0 0 24 24"><!-- a <path> --><path d="M1 2L3 4"/></svg>"#;
        assert_eq!(parse(commented).unwrap().1.len(), 1);
    }
}
