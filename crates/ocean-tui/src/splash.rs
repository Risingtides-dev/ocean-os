use std::{
    io,
    time::{Duration, Instant},
};

use crossterm::event::{self, Event};
use ratatui::{
    backend::Backend,
    layout::{Alignment, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Terminal,
};

const BANNER: &[&str] = &[
    "  d88888b     d8888b   8888888888        d8888 8888b    888 ",
    "d88P   Y88b d88P  Y88b 888              d88888 88888b   888 ",
    "888     888 888    888 888             d88P888 888888b  888 ",
    "888     888 888        8888888        d88P 888 8888Y88b 888 ",
    "888     888 888        888           d88P  888 8888 Y88b888 ",
    "888     888 888    888 888          d88P   888 8888  Y88888 ",
    "Y88b   d88P Y88b  d88P 888         d8888888888 8888   Y8888 ",
    "  Y88888P     Y8888P   8888888888 d88P     888 8888    Y888 ",
];

const OCEAN_GRADIENT: &[Color] = &[
    Color::Indexed(17),
    Color::Indexed(19),
    Color::Indexed(25),
    Color::Indexed(31),
    Color::Indexed(38),
    Color::Indexed(44),
    Color::Indexed(50),
    Color::Indexed(87),
];

const FADE_GRADIENT: &[Color] = &[
    Color::Indexed(17),
    Color::Indexed(18),
    Color::Indexed(19),
    Color::Indexed(20),
    Color::DarkGray,
    Color::Indexed(238),
    Color::Indexed(236),
];

const HOLD_MS: u64 = 700;
const SLIDE_MS: u64 = 700;
const FRAME_MS: u64 = 16;

fn banner_lines(fade_step: usize) -> Vec<Line<'static>> {
    let palette = if fade_step == 0 {
        OCEAN_GRADIENT
    } else {
        let idx = (fade_step - 1).min(FADE_GRADIENT.len() - 1);
        std::slice::from_ref(&FADE_GRADIENT[idx])
    };
    BANNER
        .iter()
        .enumerate()
        .map(|(i, raw)| {
            let color = palette[i * palette.len() / BANNER.len()];
            Line::from(Span::styled((*raw).to_string(), Style::default().fg(color)))
        })
        .collect()
}

fn banner_width() -> u16 {
    BANNER.iter().map(|l| l.len()).max().unwrap_or(0) as u16
}

pub fn play<B: Backend>(terminal: &mut Terminal<B>) -> io::Result<()> {
    let banner_h = BANNER.len() as u16;
    let banner_w = banner_width();

    let start = Instant::now();
    let hold = Duration::from_millis(HOLD_MS);
    let slide = Duration::from_millis(SLIDE_MS);
    let total = hold + slide;
    let frame = Duration::from_millis(FRAME_MS);

    loop {
        if event::poll(Duration::from_millis(0))? {
            if let Ok(Event::Key(_)) = event::read() {
                break;
            }
        }

        let elapsed = start.elapsed();
        if elapsed >= total {
            break;
        }

        terminal.draw(|f| {
            let area = f.area();
            if area.width < banner_w || area.height < banner_h {
                return;
            }

            let center_y = area.height.saturating_sub(banner_h) / 2;

            let (y, fade_step) = if elapsed < hold {
                (center_y, 0usize)
            } else {
                let t = (elapsed - hold).as_secs_f32() / slide.as_secs_f32();
                let eased = t * t;
                let travel = center_y as f32 * eased;
                let y = (center_y as f32 - travel).max(0.0) as u16;
                let step = (eased * (FADE_GRADIENT.len() as f32)).floor() as usize + 1;
                (y, step.min(FADE_GRADIENT.len()))
            };

            let x = area.width.saturating_sub(banner_w) / 2;
            let rect = Rect::new(x, y, banner_w, banner_h);

            let para = Paragraph::new(banner_lines(fade_step)).alignment(Alignment::Left);
            f.render_widget(para, rect);
        })?;

        std::thread::sleep(frame);
    }

    Ok(())
}
