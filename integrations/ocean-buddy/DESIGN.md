# Ocean Buddy Design

## Theme

A true-black Ocean stage for quick foreground voice use. Chrome stays restrained while idle and becomes bioluminescent only when connecting, listening, or presenting a result.

## Tokens

- Background `#060606`; raised `#0A0A0A`; elevated `#141414`; well `#23252B`
- Primary text `#FAFCFF`; secondary `#B8B9BB`; muted `#909098`
- Ocean accent `#00D7D7`; live accent `#00FFD7`; deep Ocean `#0087AF`
- Error `#FF4D67`; warning `#FFB224`

The Ocean depth ramp belongs to the wave mark and live state. Controls use solid fills. Ordinary surfaces use neutral depth and subtle top-edge light, not gradients or broad shadows.

## Typography

Use Apple system typography and semantic Dynamic Type styles. Headlines are concise and sentence case. Monospaced text is reserved for endpoint or diagnostic values inside settings.

## Geometry

Use 10–14 pt corner radii for raised surfaces, capsules only for compact status and the primary action, and circles for the Ocean mark and icon controls. Maintain at least 44 pt iPhone and 40 pt watchOS touch targets.

## Layout

### iPhone

1. Compact Ocean Buddy identity and connection status
2. Centered animated Ocean wave mark
3. One state headline and one supporting sentence
4. Transcript or bounded component result only when content exists
5. Persistent bottom start/stop action

Connection and session configuration open from a settings button in a sheet. They never occupy the primary voice screen. Setup happens by pairing — scanning the desktop QR code — not by typing; manual fields remain as fallback. On the Watch, a pending approval card preempts at the top of the scroll.

### Apple Watch

Use the same hierarchy in a compressed edge-to-edge layout: status, wave mark, state, primary action. Show transcript or component content only when present. Put connection configuration behind a small settings control.

## Motion

Motion expresses transport state: a slow wave drift, a warning-accented ring while connecting, and audio-responsive breathing while listening. Stop nonessential motion when Reduce Motion is enabled. Use short native state transitions; no ornamental bounce.

## Components

- `OceanWaveMark`: product identity and listening-state visualization
- `BuddyStatusLabel`: icon plus text; never color-only
- Primary voice control: solid Ocean accent when starting, neutral elevated stop control when live
- `BuddyCardView`: bounded inert result surface; model-authored content never creates device actions
- Approval card (Watch): warning-accented actionable card for the typed capability contract only — swipe right approves, swipe left dismisses, with ≥40 pt buttons as the accessible equivalent
- Outcome card (Watch): success uses the live accent, failure uses the error accent, both with one dismiss control
- Connection settings: native Form in a modal sheet

## Accessibility

Every icon-only control has a label and hint. State changes are announced through text, not just animation. Layouts must survive larger text, VoiceOver, Reduce Motion, and small 40 mm watch screens.
