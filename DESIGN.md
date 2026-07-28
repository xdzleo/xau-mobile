# DESIGN.md — Xbox Rewards

The contract for how this app looks and behaves. `CLAUDE.md` says how to
work; this says what to build. Read it before writing any screen.

Tokens alone do not produce a good app — an agent handed only colours and
spacing will put the brand colour behind a card, shadow things that
should sit flat, and give every element the same weight. So most of this
file is **judgment rules**, not values.

---

## 1. What this app is

A companion for Microsoft Rewards on Xbox. It does three things: keeps
several accounts signed in, shows what each has earned and what is
waiting, and runs a background presence farm that earns more.

**The user is a min-maxer.** They installed a third-party tool to squeeze
points out of a loyalty programme. They will open this app several times
a day, for ten seconds at a time, to answer one question: *is there
anything to collect?* Design for the ten-second glance, not the browse.

**The numbers are the product.** Points, minutes, levels. Everything else
on screen exists to give those numbers context or to act on them.

## 2. Reference apps

Not "make it nice" — these three, for these specific reasons:

| App | What we take |
| --- | --- |
| **Revolut** | Money as the hero. A balance that animates when it changes, cards that feel like objects, restraint everywhere around the number. |
| **Linear** | Typographic discipline and density. Tight tracking on large text, a real type scale, no decorative chrome. |
| **Things 3** | The courage to leave space empty. Nothing on screen that is not doing work. |

What we do **not** take from anywhere: gradients as decoration, glassmorphism,
neon glow, floating blobs, hero illustrations.

## 3. Judgment rules

These override anything else in this file when they conflict.

**Elevation.** Shadow only on things that genuinely float above the
content: bottom sheets, dialogs, the FAB, a dragged item. A card in a
list NEVER has a shadow — separate it with space and, if it must be
bounded, a 1px hairline. Compose: `Surface` with `tonalElevation`, not
`shadowElevation`, for anything that is not floating.

**Accent colour.** Xbox green is for one thing per screen: the primary
action, or the single most important state. Never on body text, never on
a decorative icon, never on a divider, never as a card background. A
screen with green in four places has no emphasis at all.

**Hierarchy.** Establish it with size and weight first, then spacing,
then colour — in that order. If two things look equally important,
change the type scale before reaching for a colour.

**Semantic colour is separate from the accent.** Green here means
"Xbox", not "success". Use the semantic tokens for state (waiting,
error, disabled) and do not let them borrow the brand green, or "there
are points waiting" becomes indistinguishable from "this is a button".

**Motion.** Springs, not linear easing. Motion communicates causality:
a number that changed should be visibly the same number that moved. If
an animation does not tell the user something, delete it.

**Density.** One spacing scale, no exceptions: 4 / 8 / 12 / 16 / 24 / 32.
Inconsistent gaps are the single loudest signal of a cheap app.

**Numbers.** Always tabular figures. Always locale-formatted (pt-BR:
`12.450`). A figure that has never been fetched renders as `—`, never as
`0` — a row claiming zero points it never asked about is a lie.

## 4. Type

**Roboto is banned.** It is the Android default and reads as
"unstyled". Ship a variable **Inter** in `res/font` and set it as the
whole M3 typography.

Scale — do not invent sizes outside it:

| Role | Size / weight / tracking |
| --- | --- |
| Hero number | 56sp / 800 / −2% |
| Screen title | 24sp / 700 / −2% |
| Card figure | 32sp / 700 / −2% |
| Section heading | 17sp / 650 / −1% |
| Body | 15sp / 400 |
| Row title | 15sp / 600 |
| Secondary | 13sp / 400 |
| Caption | 11sp / 400 |
| Label (uppercase) | 11sp / 600 / +14% |

Negative tracking on large text is what makes a big number look designed
rather than merely large. Positive tracking on small uppercase labels is
what keeps them from looking like a mistake.

## 5. Colour

Dark only. This app is used at night, next to a console.

```
ink          #0B0F0D   ground — near-black with a green bias, never neutral #111
surface      #131A16   cards
surfaceHigh  #1B231E   pressed / raised
outline      #26312A   hairlines

xboxGreen    #107C10   brand. Primary action only.
greenBright  #16C60C   on-green states, the live dot

waiting      #F0A02C   points claimable — deliberately NOT the brand green
danger       #E5705F

textPrimary  #E8EFE9
textSecond   #A8B3AC
textMuted    #7B8880   floor for anything ≥4.5:1 on surface
```

Every text/background pair must clear **4.5:1**. Measured against
`surface`, not assumed:

| Pair | Ratio | |
| --- | --- | --- |
| textPrimary | 15.1:1 | ✅ |
| textSecondary | 8.18:1 | ✅ |
| textMuted | 4.78:1 | ✅ floor |
| waiting | 8.23:1 | ✅ |
| danger | 5.74:1 | ✅ |
| greenBright | 7.68:1 | ✅ |
| **xboxGreen** | **3.30:1** | ❌ **never as text** |

That last row is why the brand green is a **fill only**. White on
`xboxGreen` measures 5.36:1 and is the pairing to use on the primary
button. Anything that wants green *type* uses `greenBright`.

## 6. Shape and spacing

Corners: 20dp cards, 16dp buttons and inputs, 12dp small chips, full
round for avatars and switches.

Touch targets are **48dp minimum**, always — including destructive icon
buttons where the glyph is small. The label pads the target; the glyph
stays small.

Screen padding 16dp. Card padding 16–20dp. Gap between sibling cards
12dp. Section gap 24dp.

## 7. The states everyone skips

This section is not optional. It is where an app admits it is amateur.

**Loading** — skeletons that match the shape of the content they
replace. Never a centred spinner. Never a blank screen.

**Empty** — say what goes here and give the action that fills it. An
empty list with no explanation is a bug the user cannot report.

**Error** — what failed, in the user's words, and what to do. Never a
raw exception, never "Something went wrong".

**Stale** — when a refresh fails, the previous figures must be cleared,
not left on screen looking current. This app has already shipped that
bug once: a balance from ten minutes ago, with a live claim button, is
worse than showing nothing.

## 8. Micro-interaction

- Every tappable thing has a press state (scale 0.97 + ripple).
- Haptics: `LongPress` on a claim landing, `TextHandleMove` on a switch.
- A balance that changes counts up over ~600ms with easeOutCubic — the
  claim should feel like being paid, not like a value refreshing.
- Shared-element transition between an account card and its detail.
- Honour `prefers-reduced-motion` / `Settings.Global.ANIMATOR_DURATION_SCALE`.

## 9. Content

Real content in every preview. No "User 1", no lorem, no `123`. Use
actual gamertags, actual game titles, actual point values from the
captured payloads. Fake content hides exactly the layout problems that
show up in production — a `@Preview` with "Game 1" would never have
caught that "The Elder Scrolls Online" does not fit.

Voice: plain pt-BR, active, specific. A button says what happens
("Resgatar 10 pontos"), and the confirmation says it happened. Never
apologise, never hedge.

## 10. Review loop

After building a screen: run it, screenshot it, and critique the
screenshot against this file before writing more. Two rounds of that
beat ten rounds of "make it better" — the errors are visible in an image
long before they are visible in code.
