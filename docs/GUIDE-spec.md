=== RANKING ===
  Guide                        xbox-native      7.7
      signature: Claiming fires a real Xbox achievement toast. A 64dp slab springs down from the top edge, full-bleed, `slab` fill with a 3dp `volt` bottom edge, nexus glyph left in `focus` white, `RESGATADO` in Plex 
  GREENBAR                     ledger-economy   7.7
      signature: **It is a printout.** Sprocket-perforated paper edges running down both sides of the screen that scroll with the content, banded in 3-line greenbar stripes, with the accounts closed by an accountant's
  Ficha — the credit register  arcade-machine   7.3
      signature: **The eight dark slots.** Your unclaimed points sit in a fixed eight-digit register, and the digits you haven't earned are *visible* — dark blue ghost numerals sitting in their slots, waiting. `000003
  Painel TX                    instrument-panel 6.3
      signature: The eight legend caps flashing in unison. Every 180 seconds, when a heartbeat actually lands, all eight titles flood one step brighter for 120ms and settle back — and a tick latches on the gauge. You 

=== SPEC ===
# FINAL SPEC — xau-mobile home screen

**Files this replaces:** `android/app/src/main/java/com/xau/rewards/ui/theme/Theme.kt`, `.../ui/HomeScreen.kt`, `.../ui/Components.kt`, plus the `Notice` composable in `MainActivity.kt`. Requires small changes in `.../core/Native.kt` and `.../AppViewModel.kt` (§10). No Rust changes.

---

## 0. THE CALL — Guide and GREENBAR tied at 7.67

Both scored 8/7/8. The tiebreak is the brief's own diagnosis: *"it designed a generic dark app with Xbox accent colouring instead of designing from the subject's own world."* GREENBAR's anchor is continuous-feed accounting paper — its own distinctiveness judge wrote that an outsider would call the result "a ledger app, not an Xbox min-maxer's tool," and its buildability judge found the two data arguments meant to bind it to *this* app were wrong about the data. It solves the stated failure by relocating rather than by going deeper into the subject. **Guide** is buildable only for an app that broadcasts presence on eight pinned console titles.

So Guide is the spine. Four things are grafted in from the runners-up because they are strictly better, and every named fatal flaw is fixed:

| Grafted | From | Replaces in Guide |
| --- | --- | --- |
| Digit **re-strike** — only the glyphs that changed snap bright and settle | GREENBAR | Deleting the count-up and animating nothing |
| **Dot leaders** as the loading form; a missing figure is visibly missing | GREENBAR | An unspecified loading state |
| **All eight tiles light together**, because `send_heartbeat_bulk` posts all eight in one call | Ficha | Guide's per-tile presence edge, which is fake data |
| **Latching** ping marks — the rail becomes a record of work done, not a progress bar | Painel TX | A plain elapsed rail |

Fatal flaws fixed: the binary hero gets a **third state** (§7); the fake per-tile edge is gone; the cycle is **900s / 5 pings**, not 960/6; the tile grid is two `Row`s, not a `LazyVerticalGrid` inside a scroll; nothing is pinned, so font scale scrolls instead of colliding; claim semantics, the busy state and the refusal states are specified; the resting state gets its own identity carrier with a measured layout budget.

---

## 1. WHY THIS ONE

The Xbox Guide overlay, the dashboard tile grid and the achievement toast are the three objects this platform actually made, and each one maps onto something this app already has — a panel of lighter material over a darker ground, a grid of sharp tiles for the eight titles, and a toast for the moment you get paid. It fixes the four things the audit measured: the hero stops being a 56sp balance nobody opened the app to read and becomes a band that is bright or dark; separation stops being a 1.09:1 fill propped up by a hairline and becomes a 1.65:1 value step with no border anywhere; selection stops being a 1.10:1 fill change and becomes a near-white focus rectangle at 8.08:1; and `#107C10`, currently the dominant chromatic note in six places, leaves the UI entirely. It also puts on screen the only content in this app that could not appear in any other app — the eight title IDs, the ping count, the cycle clock and the `Farming`/`Pinging` split, all of which are already fetched and all of which are currently thrown away.

---

## 2. PALETTE

Six structural colours, three signal colours, one text ramp. All ratios computed sRGB and verified; **the text floor is measured against `slabHigh`, the lightest surface**, so the `textMuted`-on-`surfaceHigh` 4.35:1 failure cannot recur by construction.

### Surfaces

| token | hex | RGB | channel spread | vs `carbon` |
| --- | --- | --- | --- | --- |
| `carbon` | `#0A1220` | 10,18,32 | **22/255 (8.6%)** | ground |
| `slab` | `#2E3B4D` | 46,59,77 | 31/255 | **1.65:1** |
| `slabHigh` | `#3A4A60` | 58,74,96 | 38/255 | **2.08:1** |
| `rule` | `#465873` | 70,88,115 | 45/255 | 2.59:1 |

`carbon` is a blue-cast graphite at 8.6% channel spread. The current `ink #0B0F0D` is 1.6% — a neutral grey with a comment claiming a green bias. `slab` at 1.65:1 is what makes a block an object **with no border**; the current 1.09:1 is why every container needed a `1.dp` outline. `rule` is a line weight, never a fill.

### Text and signal

| token | hex | role | on `carbon` | on `slab` | on `slabHigh` |
| --- | --- | --- | --- | --- | --- |
| `focus` | `#EEF3F8` | primary text **and** the focus rectangle | 16.79 | 10.17 | **8.08** |
| `dim` | `#C2CEDC` | secondary text | 11.74 | 7.12 | 5.65 |
| `muted` | `#ADBACB` | registers, IDs, units — **the floor** | 9.52 | 5.77 | **4.58** ✅ |
| `volt` | `#DFF23A` | points claimable. Fill and figure. | 15.09 | 9.14 | 7.26 |
| `pulse` | `#FFA0A8` | a read that failed or is stale | 9.68 | 5.87 | **4.66** ✅ |
| `presence` | `#4CC15A` | **FILL-ONLY** — broadcasting, nothing else | 8.11 | 4.91 | 3.90 |

**Inverted plates.** On a `volt` fill, every glyph is `carbon`: **15.09:1**. That value inversion — dark type on a light field — is what makes the claim band read as signage rather than as neon, and it is the reason `volt` is not a glow.

**FILL-ONLY colours:** `presence` (never type; 4.91:1 vs `slab` and 3.90:1 vs `slabHigh` both clear WCAG 1.4.11's 3:1 for non-text UI), `slab`, `slabHigh`, `rule`.

**`#107C10` appears nowhere in the UI.** It measures 3.49:1 on `carbon` and it is the swatch that produced the failure being corrected. It survives in exactly two places: `res/values/colors.xml` for the launcher icon, and inside gamerpics Xbox itself returns. The nexus glyph appears twice on screen — in the system strip and in the claim toast — tinted `muted` and `focus` respectively, never green.

**Semantic partition, enforced.** `volt` = there are points on the table. `presence` = a heartbeat is being broadcast. `pulse` = a figure could not be read. No colour carries two meanings and no meaning has two colours. Colour is never the sole carrier of a state (see §9, rule 6).

---

## 3. TYPE

Two families, both SIL OFL 1.1, both from Google Fonts, shipped as **static** files in `res/font` — Compose resolves a `FontFamily` on weight+style only, so a variable width axis inside one family collides. Each width instance gets its own `FontFamily`.

**Display — Archivo** (Omnibus-Type, OFL 1.1, fonts.google.com/specimen/Archivo). The width is the idea: every 10-foot UI Microsoft ships is *wider* than phone type, because wide letterforms with open counters survive distance. At `Expanded` it is unmistakably not Inter. Files: `Archivo_Expanded-SemiBold.ttf`, `Archivo_Expanded-Bold.ttf`, `Archivo-Regular.ttf`, `Archivo-SemiBold.ttf`, `Archivo_SemiCondensed-SemiBold.ttf`.

**Register — IBM Plex Mono** (IBM, OFL 1.1). Files: Regular 400, Medium 500, SemiBold 600, Bold 700.

```kotlin
private val ArchivoExp      = FontFamily(Font(R.font.archivo_expanded_semibold, FontWeight.W600),
                                         Font(R.font.archivo_expanded_bold,     FontWeight.W700))
private val Archivo         = FontFamily(Font(R.font.archivo_regular,   FontWeight.W400),
                                         Font(R.font.archivo_semibold,  FontWeight.W600))
private val ArchivoSemiCond = FontFamily(Font(R.font.archivo_semicondensed_semibold, FontWeight.W600))
private val PlexMono        = FontFamily(Font(R.font.ibm_plex_mono_regular,  FontWeight.W400),
                                         Font(R.font.ibm_plex_mono_medium,   FontWeight.W500),
                                         Font(R.font.ibm_plex_mono_semibold, FontWeight.W600),
                                         Font(R.font.ibm_plex_mono_bold,     FontWeight.W700))
```

**The split rule, so a builder cannot drift:** *if the system produced it, it is Plex Mono; if it would have been printed on the panel, it is Archivo.* Points, balances, gamerscores, XUIDs, title IDs, clocks, cycle numbers, ping counts, state words, block headers → mono. Gamertags, sentences, dialogs, verdict lines, tile names, button words → Archivo. Never both inside one cell. **Plex Mono never carries a sentence** — the moment it does, this is hacker cosplay.

**Tracking is never negative.** DESIGN.md §4's "negative tracking makes a big number look designed" is the fintech-hero move that produced the current screen. Display runs 0 to +1%; mono headers run +10%.

### The scale

| role | family | size | weight | tracking | line-height |
| --- | --- | --- | --- | --- | --- |
| `verdict` | ArchivoExp | 28sp | 700 | +0.005em | 1.12 |
| `bandFigure` | PlexMono | 40sp | 700 | 0 | 1.00 |
| `bandAction` | ArchivoExp | 20sp | 700 | +0.01em | 1.10 |
| `rowTag` | Archivo | 16sp | 600 | 0 | 1.25 |
| `rowFigure` | PlexMono | 18sp | 600 | 0 | 1.20 |
| `rowDelta` | PlexMono | 13sp | 600 | 0 | 1.20 |
| `blockAction` | Archivo | 15sp | 600 | 0 | 1.30 |
| `body` | Archivo | 15sp | 400 | 0 | 1.45 |
| `stateWord` | PlexMono | 13sp | 600 | +0.04em | 1.20 |
| `tileName` | ArchivoSemiCond | 12sp | 600 | 0 | 1.15 |
| `register` | PlexMono | 11sp | 500 | +0.02em | 1.45 |
| `header` | PlexMono | 10sp | 600 | +0.10em | 1.40 |
| `tileId` | PlexMono | 9sp | 400 | +0.02em | 1.10 |

Line-height in `sp` = `size × factor`, `LineHeightStyle.Trim.None`, tracking in `em` so it scales with the user's font size.

**Gamerscore is a compound, not a string.** Ship `res/drawable/ic_gamerscore.xml` (the achievement G) and render `1.204.860 Ⓖ` with the glyph inline at cap height via `InlineTextContent`, 11dp, tinted `muted`. Fallback if the asset is not drawn: a literal `G` in ArchivoExp 11/700. This is the one number in this world with cultural weight; it stops being an 11sp afterthought.

---

## 4. LAYOUT

**Full-bleed. 0dp corners. No card, ever.** Blocks run 0 → screen width; the 20dp gutter applies to type *inside* a block. Radius scale in full: `0dp` for structure, `2dp` for tile seams and the rocker. Nothing is pinned — **the whole page is one scrolling column**, so raising the system font size scrolls rather than collides.

Spacing scale unchanged: 4 / 8 / 12 / 16 / 20 / 24 / 32 (20 is added and is the block gutter). No literals outside it.

Measured budget at 360×800dp, insets ~48dp, 2 accounts:

```
system strip      36
verdict band      96   (clear) / 112 (waiting)
balance row       44
CONTAS header     28
accounts block   184   (64 + 64 + 56)
PRESENÇA header   28
presence block   272
                 ----
                 688dp of 752dp usable
```

At 3 accounts: 752dp. **The tile grid is above the fold at rest for up to three accounts.** That is a hard requirement, not a coincidence: the resting state must carry the app's identity without any signal colour on screen, and the tiles are what carry it.

### 1 — System strip · 36dp · `slab` fill · full bleed

`ic_xbox` 14dp `muted` · `XAU` (header) · `3 CONTAS` (header, `muted`) — left group, 8dp gaps.
Right: `189.40.12.7` (register, `muted`), 8dp, then a 6dp square — `presence` fill + `TX` when the farm is broadcasting, `rule` fill + `OFF` when not. Replaces `Lockup()` and the orphaned IP line at the bottom.

### 2 — The verdict band · full bleed · the hero

**It is a state, not a number.** From across a room the app is dark or it is a bright bar, and that answers the ten-second question before a glyph is read. The band **is** the claim control; there is no separate button, which removes the giant-number-plus-green-button pair entirely.

**WAITING** — fill `volt`, all type `carbon`, `heightIn(min = 112.dp)`, padding 20dp.
- Row, baseline-aligned: `+2.340` (bandFigure) left · `RESGATAR` (bandAction) end.
- 8dp.
- `EM 2 CONTAS · GBOSS77 · XDZLEO` (register). Append ` · 1 SEM LEITURA` when any account's `unclaimed` is null.
- Tap → §4.6. Press → travels down 2dp, no scale.

**CLEAR** — fill `slab`, 4dp leading blade in `rule`, `heightIn(min = 96.dp)`, padding 20dp. This is the state the user sees most, so it is the best-designed one.
- `Nada a resgatar` (verdict, `focus`) — 15 glyphs at ArchivoExp 28/700 ≈ 250dp inside 320dp. Two lines allowed; must be checked in the preview.
- 6dp.
- `ÚLTIMA LEITURA 14:32 · PRÓXIMA EM 00:47` (register, `muted`). While `state.refreshing`, the second clause reads `LENDO…`.
- Not tappable.

**UNKNOWN** — fill `slab`, 4dp leading blade in `pulse`, 96dp.
- `Leitura falhou` — or `Sem leitura` when nothing was ever fetched — (verdict, `focus`), with `TENTAR` (bandAction, `focus`) at the end of the row.
- `<motivo> · ÚLTIMA LEITURA 14:32` (register, **`pulse`**), from `state.fatal` ?: the first non-null `staleReason`. When never fetched: `SEM LEITURA · TOQUE PARA TENTAR`.
- Tap → `refresh()`. **`state.fatal` finally has a home on the signed-in path.**

**BUSY** — the WAITING band with `RESGATANDO…` in place of `RESGATAR`, tap disabled, fill unchanged.

### 3 — Balance row · 44dp · on `carbon` · 1dp `rule` above and below · 20dp gutter

Left: `SALDO` (header, `muted`) — `SALDO TOTAL` when more than one account.
End: `61.130` (rowFigure, `focus`) · 8dp · `1.204.860 Ⓖ` (register, `muted`).
When any `balance` is null the figure is `—` in `muted` and `1 SEM LEITURA` (register, `pulse`) sits to its left. A total that silently sums only the accounts that answered is a more confident lie than an em dash.

A stat line, not a card. The balance answers a question nobody opened the app to ask.

### 4 — `CONTAS` header · 28dp

`CONTAS` (header, `muted`) at the 20dp gutter, then a 1dp `rule` running from the end of the text to the screen edge. This is a table header, not a floating label — it is the one item from the disqualified list that survives, and only in this changed form.

### 5 — Accounts · one `slab` block · N rows of 64dp · 1dp `rule` **between** rows only

Row, leading to trailing:
- **4dp full-height blade.** `volt` if `unclaimed > 0`; else `pulse` if `staleReason != null`; else `presence` if `farming`; else `rule`. Precedence in that order.
- 12dp · **40dp square gamerpic, 0dp radius** (the circle is the phone tell; console gamerpics are square). Fallback: `slabHigh` square, first letter in ArchivoExp 18/700 `dim`.
- 12dp · Column, `weight(1f)`: gamertag (rowTag, `focus`, 1 line, ellipsis) · 2dp · `1.204.860 Ⓖ` (register, `muted`) — or `staleReason` (register, `pulse`, 1 line, ellipsis) when stale.
- Column, end-aligned: balance (rowFigure, `focus`; `—` in `muted` when null, `—` in `pulse` when stale) · 2dp · `+340` (rowDelta, `volt`) when waiting, or `TX` (register, `presence`) when farming and not waiting.
- 20dp trailing.
- **Selected:** fill `slabHigh` **plus a 2dp `focus` rectangle** around the row at **8.08:1**. This is the only border in the app.
- Tap → `select(xuid)`. Long press → the existing removal dialog, restyled: `slab` container, 0dp corners.
- Press feedback: fill steps to `slabHigh` (or `rule` if already `slabHigh`). **No scale** — a 0.97 scale on a full-bleed row leaves a gap at the screen edge and reads as breakage.

Last row of the same block, 56dp, 1dp `rule` above: `+ ADICIONAR CONTA` (blockAction, `dim`), start-aligned at the 20dp gutter. Not a dashed box, not a card.

### 6 — `PRESENÇA` header · 28dp — same header+rule form

### 7 — The presence block · one `slab` block — the farm, finally on screen

**Row A · 56dp · 20dp gutter.**
- Left: the state word from `FarmState.state` (stateWord): `TRANSMITINDO` (`presence`) when `pinging` · `EM CICLO` (`focus`) when `farming` · `PARADO` (`muted`) when `idle`/`stopped` · `ERRO` (`pulse`) when `error`. **This is the `Farming`/`Pinging` split that is currently collapsed into a boolean and discarded.**
- End: a **two-position rocker plate**, 112×48dp, 2dp radius, two 56dp halves labelled `DESL` and `TX` (stateWord). The engaged half fills `slabHigh` with a 3dp `presence` bottom edge and a `focus` label; the idle half is `slab` with a `muted` label. One tap target for the whole plate → `toggleFarm()`. Haptic `TextHandleMove`. Set `Role.Switch` and a `stateDescription` so TalkBack still reads it as a switch.

**1dp `rule`.**

**Row B · the tile grid.** Two `Row`s of four, each tile `weight(1f)`, 2dp seams, `heightIn(min = 72.dp)`, `slabHigh` fill, 0dp corners, 8dp padding. **Not a `LazyVerticalGrid`** — eight items are known at compile time and a lazy grid inside a scrolling column crashes on infinite height constraints.
- Top of tile: raw decimal title ID (tileId, `muted`).
- Bottom of tile: title name (tileName, `dim` when broadcasting, `muted` when stopped), `maxLines = 2`, ellipsis.
- **When the farm is broadcasting, all eight carry a 3dp `presence` bottom edge, together.** `send_heartbeat_bulk` posts all eight title IDs in one call — there is no rotation, and a visual implying one would be decoration wearing a data costume.
- Order is exactly the core's order (`FIXED_CONSOLE_TITLES`, console-exclusives first): `331135949` Doom Eternal · `1790482525` Call of Duty: Black Ops Cold War · `2043073184` Halo Infinite · `2030093255` Forza Horizon 5 / `374923716` Gears 5 · `2079794073` Starfield · `1842668909` Forza Motorsport · `66591171` Sea of Thieves.
- **Tiles are not tappable.** They are a display. The eight are pinned in Rust; nothing in the copy may suggest a ninth can be added.

**1dp `rule`.**

**Row C · the cycle rail · 24dp · 20dp gutter.** The cycle is **900s = 15 minutes** (`FARM_CYCLE_SECS`), not 16.
- 15 segments, one per minute, each `weight(1f)`, 10dp tall, 3dp seams, 0dp corners. Elapsed → `presence`. Current minute → `focus`. Remaining → `rule`.
- **Five latching ping marks** at the leading edge of segments 0, 3, 6, 9 and 12 — 2dp wide, 16dp tall. The core pings at t = 0/180/360/540/720 and breaks the cycle at 900 *before* the ping branch, so there are exactly five, not six. A mark is `rule` until its ping lands and **`focus` once it has**, and it stays latched for the rest of the cycle. `landed = if (running) min(5, elapsedSecs / 180 + 1) else 0`. The rail is a record of work done, not a progress bar.

**Row D · the register · 20dp tall · 20dp gutter · register style.**
- Running: `CICLO 0034 · 07:12 / 15:00 · 271 PINGS`, `muted`. Append ` · 2 ERROS` in `pulse` **only when `errors > 0`**.
- Stopped: `PARADO · 8 TÍTULOS · CICLO DE 15 MIN`.
- Farm error: the truncated `last_error`, in `pulse`.

### 4.6 — Claim semantics

The band is global, so it claims globally: **`Api.claimAll()`**. That matches `maybeAutoClaim()`, which already sweeps every account without a tap, and it removes the current design's need for a "pontos aguardando em outra conta" apology line — the band already says `EM 2 CONTAS` and names them. There is no per-row claim control; a row's job is selection.

---

## 5. THE SIGNATURE

**The presence board: eight sharp tiles with raw decimal title IDs printed on them, over a fifteen-segment rail with five latching ping marks.**

The claim toast (§6) is the more *available* Xbox idea and it is genuinely good — but it fires only on a claim, and the state this user sees on most of their several-times-a-day opens is "nothing waiting". An identity that lives entirely in an event leaves the resting screen to structure alone. So the signature is the thing that is on screen at rest, above the fold, every single time: `2043073184 / HALO INFINITE` next to `1790482525 / CALL OF DUTY: BLACK OPS COLD WAR`, eight of them, sharp-cornered, with a green edge under all eight when the machine is broadcasting and none when it is not, and a rail below that you can count minute by minute and watch a mark latch every three minutes.

Nothing else on a phone prints a decimal Xbox title ID on a tile. No crypto dashboard, no generic dark app, no generated layout does this — because no other app has eight pinned title IDs to print. It is also the only element that could not survive being lifted into a different product.

Build it exactly as §4.7 Rows B and C describe. Nothing about it is decorative: every tile is a title the app is actually broadcasting, every mark is a heartbeat that actually landed.

---

## 6. MOTION

Springs only. **Nothing loops.** Every animation reports something; if it does not, it is deleted.

| what | when | spec | what it says |
| --- | --- | --- | --- |
| **Focus rectangle travel** | account selection changes | `spring(0.72, 700f)` on the rect's Y offset | D-pad focus crossing the rail — selection moved |
| **Digit re-strike** | any mono figure's formatted string changes | changed glyph indices snap to `focus` and settle to their resting colour over 260ms; unchanged glyphs do not move | *which* digits changed. Monospace makes positions fixed, so this costs less than the count-up it replaces — and a count-up is the most-copied fintech microinteraction in existence |
| **Band latch** | CLEAR/UNKNOWN → WAITING | 90ms hard cut, no crossfade | lamps do not fade in |
| **Band drain** | claim succeeds | `volt` → `slab` over 220ms while the toast lands | the credit was filed |
| **Ping flash** | observed `ping_count` delta while resumed | all 8 tile bottom edges → `focus` for 140ms, settle to `presence` over 260ms `spring(0.7, 700f)`; the rail mark latches | a real heartbeat left the device — the first time `pings` has ever reached the screen |
| **Rail advance** | once per minute | one segment, `spring(0.8, 900f)`, 240ms | the only motion tied to time |
| **The toast** | see below | `spring(0.75, 500f)` in, hold 2600ms, spring out | you were paid |
| **Press travel** | band, rocker | translate +2dp Y, `spring(0.5, 1400f)` | a plate depressing |

**The claim toast — one overlay system, not two.** A 64dp full-bleed `slab` slab springs down from the top edge under the status-bar inset, with a 3dp bottom edge, `ic_xbox` 20dp `focus` at the start, then `RESGATADO` (header) over `+2.340 pontos` (bandAction, `focus`). Haptic `LongPress` on landing. **This component carries every `state.notice`**, with the edge colour set by tone: `volt` for a credit, `rule` for neutral (`Conta removida`), `pulse` for a refusal (`Nada foi creditado`, `Resgate automático falhou — pausado 30 min`). Auto-claim fires without a tap, so this toast is often the only record the user sees of an event — it must therefore cover the unhappy paths, not just the win. Delete `MainActivity.Notice`.

**Ping-flash seeding.** On resume, the first `farmState` observation seeds `previousPingCount` **without animating**. A flash that fires because the screen woke up is noise, not causality.

**Reduced motion.** Read `Settings.Global.ANIMATOR_DURATION_SCALE` once at theme level into a `LocalReducedMotion`. At 0: focus rect snaps; re-strike renders a single frame at the settled colour; the ping flash is skipped but the rail mark still latches; the band drain is a cut; the toast appears and holds 2600ms statically. Every state stays legible because no information was ever carried by motion. This is currently promised by DESIGN.md §8 and read nowhere.

---

## 7. THE STATES

**Loading (`booting`).** No shimmer, no spinner, no skeleton block. The screen is a register with no reading yet:
- Band = the UNKNOWN shape, verdict `Lendo…`, no reason line, no `TENTAR`, blade `rule`.
- Balance row value = `—` (`muted`).
- Account rows: two rows with a `slabHigh` square where the gamerpic goes and a **dot leader** `··········` (register, `rule`) filling each value column at the width the figure will occupy. Shaped like the content, completely static.
- **The tile grid renders in full with real names and real IDs**, dark, no presence edge — `farmTitles()` is a compiled-in constant list and needs no network. The most distinctive element on the screen is present in the very first frame.
- Rail all `rule`. Register: `LENDO…`.

**Empty (no accounts).** Routes to `Welcome`, restyled into this system: full-bleed, 0dp corners, no card, `Nenhuma conta conectada` in `verdict`, the explanation in `body`/`dim`, and `ENTRAR` as a `volt` band with `carbon` type — the same object as the claim band, because it is the same gesture. **The presence block renders below it in its stopped state**, so first-run shows what the app actually does.

**Error (fatal, signed in).** Band = UNKNOWN, `state.fatal` verbatim in the reason line (`pulse`), `TENTAR` → `refresh()`. `state.fatal` is currently set by `refresh()` and `signOut()` and rendered only on the signed-out path; a signed-in user whose refresh dies currently gets nothing.

**Stale (per-account).** Row blade `pulse`; the figure column prints `—` in `pulse` — a missing number is visibly a missing number, not a zero; the sub-line under the gamertag carries `staleReason` in `pulse`.

**The band's tri-state — this is the fix for the flaw two judges called fatal.** `AppViewModel` coerces null to zero (`sumOf { it.unclaimed ?: 0L }`) and a quiet poll failure leaves `fatal` untouched, so a two-state band would render "Nada a resgatar" at 28sp, full bleed, as a confident wrong answer. It never may:

```kotlin
val waitingAccts = accounts.filter { (it.unclaimed ?: 0L) > 0L }
val unknownAccts = accounts.filter { it.unclaimed == null }
val band = when {
    waitingAccts.isNotEmpty() -> Band.Waiting(
        total   = waitingAccts.sumOf { it.unclaimed!! },
        tags    = waitingAccts.map { it.gamertag },
        unknown = unknownAccts.size,
    )
    fatal != null || unknownAccts.isNotEmpty() -> Band.Unknown(
        reason = fatal ?: unknownAccts.firstNotNullOfOrNull { it.staleReason },
        lastReadMs = lastReadMs,
    )
    else -> Band.Clear(lastReadMs)
}
```

**CLEAR is unreachable while any `unclaimed` is null.** That is the invariant; write a unit test for it.

**Farm running.** Rocker on `TX`; state word live; all eight tiles carry the `presence` edge; rail advancing with latched marks; register counting; system strip square `presence`. **Farm error** (`state == "error"`): state word `ERRO` in `pulse`, tiles lose the edge, register shows the truncated `last_error`.

---

## 8. WHAT TO DELETE

**`Components.kt`** — `Skeleton` (a decorative gradient, invisible at 1.19:1, with raw pixel offsets that are wrong at every size); `LiveDot` (an infinite `LinearEasing` tween carrying two unrelated meanings); the 600ms `CubicBezier` count-up inside `Figure` (keep the null → `—` guard, drop the tween); `SectionLabel`; `PrimaryButton`; `GhostButton`; `StateBlock`'s centred-card form; the `// ---- card` header with nothing under it and the `[XauCard]` KDoc reference to a composable that does not exist; the 0.97 scale in `pressable` for rows.

**`HomeScreen.kt`** — `Lockup`, `Hero`, `AccountSkeleton`, `AddAccountCard`, `FarmCard`, `CycleBar`, `Avatar`'s `RoundedCornerShape(50)`, the M3 `Switch`, the `LazyColumn` gutter architecture, and every `.border(1.dp, c.outline, RoundedCornerShape(Radius.card))` — four identical copy-pasted blocks. Also the off-scale literals: `2.dp` (316, 332, 443), `20.dp` (176), `44.dp` (361, 396), `180/56.dp` (204), `120/15.dp` (399), `70/11.dp` (400), `64/24.dp` (402), `4.dp` (503, 510).

**`Theme.kt`** — every colour token (`ink`, `surface`, `surfaceHigh`, `outline`, `brand`, `brandBright`, `waiting`, `danger`, `textPrimary`, `textSecondary`, `textMuted`); the whole `XauType` scale; the `inter()` helper; `Radius.card/control/chip` (→ `Radius.none = 0.dp`, `Radius.seam = 2.dp`).

**`MainActivity.kt`** — the `Notice` composable (absorbed by the toast).

**Assets** — `res/font/inter_variable.ttf`. In `res/values/colors.xml`, retarget `ink` to `#FF0A1220` and leave `brand` with a comment saying it is the launcher icon only and must never be read by Compose.

**Copy** — `"8 títulos de console · ciclo de 16 min"`. The cycle is 15 minutes.

---

## 9. RULES THE IMPLEMENTATION MUST NOT BREAK

Written in the register of DESIGN.md §3. These override anything above when they conflict.

**Structure is value, not line.** A block is an object because `slab` clears `carbon` by 1.65:1. **The only border in this app is the 2dp `focus` rectangle on the selected account row.** Rules run *between* rows, or from a header to the screen edge — they never close around a box. If you find yourself outlining something to make it visible, the fill is wrong; fix the fill.

**Corners are 0dp.** 2dp exists for tile seams and the rocker plate and for nothing else. There is no `Radius.card`, because there are no cards.

**One accent state per moment.** `volt` means points are claimable, `presence` means a heartbeat is broadcasting, `pulse` means a figure could not be read. A colour never carries two meanings and a meaning never gets two colours. If `volt` is on screen when nothing is claimable, that is a bug.

**`presence` is fill-only. `#107C10` is not a colour in this app.** Neither ever renders as type. `presence` clears 3:1 against both surfaces as non-text UI and does not clear 4.5:1 on `slabHigh` — the type system must not offer it as a content colour.

**The text floor is measured against `slabHigh`, the lightest surface.** `muted #ADBACB` = 4.58:1 there. Any new ink is tested against `carbon`, `slab` **and** `slabHigh` before it ships. The previous system tested one surface for an app that had three, and shipped a 4.35:1 failure on the *selected* row — the one being read.

**Colour is never the only carrier of a state.** The waiting row shows a `volt` blade *and* a `+340` figure. The farming row shows a `presence` blade *and* a `TX` mark. The stale row shows a `pulse` blade *and* an em dash *and* a reason. Deuteranopia must not cost information.

**The band may never say "nothing" when it means "I don't know".** `Band.Clear` requires every account's `unclaimed` to be non-null and `fatal` to be null. This bug has already shipped once (DESIGN.md §7); a two-state hero re-creates it structurally.

**A figure never fetched renders `—`; a figure that failed renders `—` in `pulse`; a total containing an unknown renders `—`.** A total that silently sums only the accounts that answered is a more confident lie than an em dash.

**Nothing loops.** No `rememberInfiniteTransition` anywhere in the app. Every animation is fired by an observed event and reports what that event was. If an animation does not tell the user something, delete it — including the ones in this spec.

**No negative tracking, anywhere.** Display type runs 0 to +1%, mono headers +10%. A large number set tight is the fintech hero move that produced the screen being replaced.

**Plex Mono never carries a sentence, and Archivo never carries a figure.** Five mono roles exist — IDs, counters, clocks, state words, headers — and that list is closed. The moment mono appears in prose this is a terminal, and a terminal is the crypto dashboard.

**The tile grid tells the truth.** All eight titles light together or none do, because `send_heartbeat_bulk` posts all eight in one call and `FarmState` has no per-title data. Do not invent a rotation. Do not make a tile tappable. Do not suggest a ninth title.

**Touch targets 48dp minimum**, including the rocker plate (112×48) and the add-account row (56dp). The tile grid is exempt because it is not interactive.

**Nothing is pinned.** The page is one scrolling column, heights are `heightIn(min=)` not `height()`, and every layout must be checked at `fontScale = 1.3`.

**Verification, before any of this is believed.** `grep -rn "@Preview"` over `com/xau/rewards/` currently returns zero results, and **task #31 is still pending**. Every finding in the audit was visible in an image and invisible in a diff. Required previews, with real content per DESIGN.md §9: (1) CLEAR band, 2 accounts, farm running — the resting state, the one that must be best; (2) WAITING band, 3 accounts, one of them stale; (3) UNKNOWN band from a quiet poll failure; (4) booting with dot leaders; (5) the tile grid alone, containing the literal string `Call of Duty: Black Ops Cold War` at 360dp; (6) the toast in all three tones; (7) every preview again at `fontScale = 1.3`. Then build, screenshot, critique against this file before writing more.

---

## 10. PLUMBING REQUIRED (small, and one of it is a live bug)

**`Native.kt` — `FarmState.running` is broken today.** The Rust enum is `#[serde(rename_all = "lowercase")]`, so the wire value is `"farming"` / `"pinging"`, and `Native.kt:191` compares against `"Farming"` / `"Pinging"`. It is always `false`. The `farm_start` fallback branch in the JNI hand-writes `"Idle"` capitalised, which is what hides it. Parse case-insensitively.

Also in `Native.kt`: `totalSecs` default `960L` → `900L`; decode `errors`, `lastError`, `lastPingUnix` and `titles` — all four are already serialised by `serde_json::to_value(FarmProgress)` and all four are currently dropped on the floor.

**`AppViewModel.kt`:**
- `UiState` gains `farmTitles: List<FarmTitle>` (loaded once in `init` — a pure constant list, so it is available before any network call and the tile grid is never empty) and `lastReadMs: Long?` (set on every successful `refresh`, drives `ÚLTIMA LEITURA` / `PRÓXIMA EM`).
- Replace `totalWaiting` with the tri-state in §7.
- Add a **1 Hz local farm ticker** while resumed and a farm exists: `farm_state` is a `RwLock` read and a clone with no network, so a 1s poll costs nothing. The existing 60s network poller is untouched. Without this the cycle clock, the rail and the ping flash all sample at 60s, and `Pinging` — which lasts about a second inside a 180s interval — would be observed roughly 2% of the time.
- `state.refreshing` is currently set and rendered nowhere; the band's register line renders it as `LENDO…`.

**Copy fix:** `FARM_CYCLE_SECS = 900`. Fifteen minutes, five pings per cycle at t = 0/180/360/540/720.