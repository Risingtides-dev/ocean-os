//! Hybrid page perception. Default: a cheap structured read — visible
//! interactive elements with a stable `ref` (selector) + role + text, plus the
//! page's visible text. The caller decides whether to additionally grab a
//! screenshot (visual pages). This module returns structured data only.

use serde::{Deserialize, Serialize};

/// One interactive element the agent can target by `ref`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ElementRef {
    /// CSS selector usable with click_selector.
    pub r#ref: String,
    pub role: String,
    pub text: String,
}

/// Structured page read.
#[derive(Debug, Serialize, Deserialize)]
pub struct PageRead {
    pub url: String,
    pub title: String,
    /// Visible interactive elements (links, buttons, inputs).
    pub elements: Vec<ElementRef>,
    /// Trimmed visible text.
    pub text: String,
    /// True when the page looks visual (canvas/video heavy) and a screenshot
    /// is recommended over the structured read.
    pub visual_hint: bool,
}

/// JS evaluated in the page to build a PageRead as JSON. Kept as a single
/// expression so it can be passed to page.evaluate.
pub const READ_PAGE_JS: &str = r#"
(() => {
  const sel = (el) => {
    if (el.id) return '#' + CSS.escape(el.id);
    const parts = [];
    let n = el;
    while (n && n.nodeType === 1 && parts.length < 4) {
      let p = n.tagName.toLowerCase();
      if (n.classList.length) p += '.' + [...n.classList].slice(0,2).map(c=>CSS.escape(c)).join('.');
      const sibs = n.parentNode ? [...n.parentNode.children].filter(c=>c.tagName===n.tagName) : [];
      if (sibs.length > 1) p += `:nth-of-type(${sibs.indexOf(n)+1})`;
      parts.unshift(p);
      n = n.parentElement;
    }
    return parts.join(' > ');
  };
  const vis = (el) => {
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0 && r.bottom > 0 && r.top < innerHeight;
  };
  const out = [];
  document.querySelectorAll('a,button,input,textarea,select,[role=button],[role=link]').forEach(el => {
    if (!vis(el)) return;
    const text = (el.innerText || el.value || el.placeholder || el.getAttribute('aria-label') || '').trim().slice(0,120);
    out.push({ ref: sel(el), role: el.getAttribute('role') || el.tagName.toLowerCase(), text });
  });
  const canvases = document.querySelectorAll('canvas,video').length;
  const bodyText = (document.body ? document.body.innerText : '').trim().slice(0, 6000);
  return JSON.stringify({
    url: location.href,
    title: document.title,
    elements: out.slice(0, 120),
    text: bodyText,
    visual_hint: canvases > 0 && bodyText.length < 200
  });
})()
"#;
