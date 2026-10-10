# Display list format (rtex 0.0.2, binary encoding revision 1)

A display list is what rtex hands a host to draw: positioned glyphs, rules, images, color
operations and markers, in scaled points (sp; 65536 sp = 1 pt; 65781.76 sp = 1 bp), y growing
downward. Two framings exist:

- **unit** display lists (fast-path results, still called "paragraph" lists in the API): origin
  at the top-left of the unit's box; `lines` are the unit's **rows** in order. A row is an
  hlist reached from the box through vlists only: a text line, a display-math row, a list item
  line, a float's image line or caption line, a table row's outer box. Hosts position rows with
  the `fragments` of the `ParagraphUpdate` (per-row `xs`/`baselines` in page coordinates).
- **page** display lists (background layouts): origin at the top-left of the page; `lines` carry
  the owning unit (`unit`, with the 1-based `row` index) and, for text lines, the capture
  paragraph (`par`, `i`); `other` holds page material that belongs to no unit (headers, footers,
  footnote rules, page-level color ops). Footnote text lines keep their `par` but belong to no
  unit: a fast result for the paragraph replaces its rows, not its footnote text.

Within a row, items are in content order. `other` precedes the lines.

## Binary encoding (revision 1; the header carries the revision number)

Little-endian throughout. `str` = `u16 length` + UTF-8 bytes.

```
Header (16 bytes)
  'L' 'O' 'D' 'L'      magic
  u16 version          1
  u16 flags            bit 0: 1 = page, 0 = paragraph
  u32 total_length     bytes including this header
  u32 reserved         0
Records: u8 tag, u32 payload_length, payload   (unknown tags must be skipped)
  0x01 META     i32 width, i32 height, i32 depth, i32 page, i32 page_width, i32 page_height,
                i32 origin_x, i32 origin_y, u32 glyph_count, u32 image_count
  0x02 FONT     u32 font_id, i32 size_sp, u8 kind, u8 reserved, u16 subfont, i32 slant, i32 extend,
                i32 squeeze, i32 designsize, str filename, str psname, str name, str fullname, str format
                kind: 0 unknown, 1 opentype, 2 truetype, 3 type1, 4 type3, 5 virtual (never emitted: expanded)
                slant/extend/squeeze in thousandths (extend 1000 = none; 0 = unset)
  0x03 FLAG     str key, i32 value            degradation flags (page is Degraded when any is present);
                `transformed_rows`: rows typeset inside a transformed box (a landscape page's
                table, a rotated \parbox) keep the box's own coordinates, and no record ties them
                to the MATRIX that turns them: such a page is drawn from its PDF
  0x10 LINE     i32 par, i32 line_index, i32 x, i32 baseline_y, i32 width, i32 height, i32 depth,
                f64 glue_set, u8 glue_sign, u8 glue_order
  0x12 LINE_UNIT i32 unit, i32 row          (page lists; directly after LINE) owning unit and row index
  0x11 LINE_END
  0x20 GLYPHS   u32 font_id, i32 baseline_y, i32 expansion, u32 n,
                n × { u32 char_code, u32 glyph_index (0xFFFFFFFF = none), i32 x, i32 advance }
  0x21 RULE     i32 x, i32 y_top, i32 width, i32 height
  0x22 COLOR    u8 cmd (0 set, 1 push, 2 pop, 3 current, 255 unknown), u8 reserved, u16 stack, str data
                (lists from before rtex-dl.lua read the node's `command` field carry 255)
  0x23 LITERAL  i32 mode, str data [, i32 x, i32 y]
                                             raw pdf_literal / \special (mode −1); page is Degraded.
                                             x, y: where it was output (baseline); present in lists
                                             from rtex-capture with native drawing (a decoder reads
                                             them when the payload has 8 more bytes)
  0x24 UNSUPPORTED str kind, str detail      something not representable; page is Degraded.
                                             kind `box_resource`: a \useboxresource form, detail
                                             "index x top width height"; kind `shading`: a pgf
                                             axial/radial shading form, detail "index x top width
                                             height depth <JSON spec>" (see Native drawing)
  0x25 MATH     u8 on, i32 x                 inline math boundary (hit-testing aid)
  0x26 IMAGE    i32 resource_index, i32 x, i32 y_top, i32 width, i32 height
  0x27 IMAGE_INFO u32 resource_index, u32 page, u32 pages, str file     source file of an image index
  0x28 MATRIX   u8 op (0 save, 1 set, 2 restore), i32 x, i32 y, str data
                PDF transformation state (graphicx scaling/rotation): `set` applies the matrix
                "a b c d" about the point (x, y) to everything up to the matching `restore`
  0x29 PICTURE  str key, i32 x, i32 top, i32 width, i32 height
                a picture this page draws from the picture cache's stored drawing (its
                `cached_picture` item was replaced): where it is, by its cache key
  0x2A ROTATE   i32 degrees                  the page's /Rotate (90, 180 or 270, clockwise) from
                the page attributes at shipout (pdflscape): coordinates stay those of the
                unrotated page; hosts turn the whole page as viewers do. Absent when 0
  0xFF END

In unit lists the META record's `origin_y` slot carries the number of insert nodes (footnotes,
marginal notes) the unit produced: their text is not in the list and keeps the page's version
until the next layout (`ParagraphUpdate.reasons` lists `inserts`).
```

## Rendering rules

- **Glyphs.** Draw glyph `glyph_index` of the font file `filename` (subfont `subfont` for
  collections) at `(x, baseline_y)`, scaled to `size_sp`, horizontally scaled by
  `1 + expansion / 1 000 000` (microtype font expansion; LuaTeX encodes it the same way in the
  PDF text matrix), then by `extend / 1000` when non-zero, with `slant / 1000` as horizontal
  shear. `char_code` is the Unicode/char code for text extraction and TFM fonts (`glyph_index`
  absent). `x` already includes `xoffset`; `baseline_y` includes `yoffset`. `advance` is the
  width TeX advanced by after this glyph (informational; positions are absolute).
- **Rules** are filled rectangles. LuaTeX draws them in the PDF as stroked lines of the same
  geometry; both render identically.
- **Images** reference the engine's image resource index; `IMAGE_INFO` records (page lists and
  fast results alike) give the file, page and page count. graphicx draws bitmap images at their
  natural size inside a `MATRIX save` / `set` / `restore` group: apply the matrix about its
  point to the image rectangle (`verify.rs` shows the composition). A picture the background
  pass (or the live engine) took from the picture cache is not an image: it appears as
  `UNSUPPORTED{kind: "cached_picture", detail: "<index> <x> <top> <width> <height>[ <key>]"}` (sp,
  the picture's rectangle in the list's frame; `key` is the picture's `file:line` cache key) and the page carries the `pic_cache` flag, so it is
  degraded and hosts render it from the pass PDF, exactly like a page with a drawn TikZ picture.
  In a live result the item marks where the picture now stands: a host that draws the layout
  page from the PDF can copy the picture's rectangle from that rendering (the same `width` and
  `height` identify it on the page) to the live position before it redraws the unit's rows.
- **Color** records are LuaTeX `pdf_colorstack` operations with the raw PDF color operators in
  `data` (e.g. `1 0 0 rg 1 0 0 RG`); `set` replaces the stack top, `push`/`pop` nest. A paragraph
  display list starts with a `set` of the color in force at its start when it is not black.
- **Fonts** are identified across processes by `FontDesc::key()` (file, subfont, size, slant,
  extend, squeeze); font ids are per-process.
- Pages with any FLAG, LITERAL or UNSUPPORTED record are *Degraded*: draw the PDF fallback page
  the `LayoutUpdate` names instead, unless the host draws them natively (below). MATRIX records
  do not degrade a page.

## Native drawing (TikZ / pgf pictures)

A page whose flags are only `literal` and `shading` can be drawn without its PDF:
`rtex_dl::gfx::native_graphics(&dl)` replays the page's literals, color stack operations,
MATRIX records and shading forms the way LuaTeX writes them into the PDF and returns a
`NativePage`, or the first thing it does not understand (`Unsupported`: text or XObjects in
literals, tiling patterns, other graphics states, literal modes other than 0/1, `\special`,
`box_resource`). It never returns a partial drawing. `LayoutUpdate.pages_changed[].native` carries
it for every degraded page it resolves (absent otherwise); the page stays `exact: false`, so a host
that does not draw natively keeps using the PDF.

- `ops` are in content order; each names the item (`at`: row `line`, or `null` for the page's
  `other` items, and `item` index) whose operator produced it. Draw them interleaved with the
  list's glyphs, rules and images in that order: an op tagged with item *k* goes right after
  item *k*.
- `Save` / `Restore` bracket clip changes (PDF `q` / `Q`). `Clip` intersects the clip with a path
  (nonzero or even-odd) **for everything drawn after it until the matching `Restore`, glyphs
  included**: pgfplots clips its plot area, and a label outside it is invisible in the PDF.
- `Paint` fills (nonzero or even-odd) and/or strokes (fill first) a path. `Shade` paints a linear
  (`coords` x0 y0 x1 y1) or radial (x0 y0 r0 x1 y1 r1) gradient over `bbox`, with linear color
  `stops` (offsets 0..1; `extend` continues the end colors) and opacity `alpha`; a host maps it
  to its gradient primitive.
- Paths and gradients are in user space (PDF units, y up); `ctm` maps them to the list's frame
  (sp, y down). Stroke widths and dashes are in user space too. Set the transform, then draw.
- Colors are `{comps, alpha}` with 1 (gray), 3 (RGB) or 4 (CMYK) components as the PDF gives
  them; convert CMYK the way the host's color management does (`Color::rgb` is the plain
  formula; MuPDF differs by up to a few percent).
- Pictures the background pass took from the picture cache keep their drawing: the cache stores
  each picture's display-list items with its PDF region, and a page list puts them back in place
  of the `cached_picture` item (`pic_cache` flag gone when every cached picture on the page came
  back), so a layout is drawn natively whether its pictures were typeset or cached. The page
  lists each such picture in `pictures` (PICTURE records: key and rectangle). Live (fast-path)
  results keep `cached_picture` items, whose detail ends with the same key: a host that copies a
  cached picture into a live unit finds its pixels on the page by key.
- `transforms` lists glyphs, rules and images that a transformed pgf scope (rotated or scaled
  node text, axis labels) moves away from their list position: draw them with that map (list
  coordinates to list coordinates) instead of their MATRIX records.

`scripts/gfx_compare.py` checks a capture's native drawing against MuPDF's reading of the PDF
(every fill and stroke by type, geometry, color, width and opacity; display-list rules; moved
glyph origins) and `scripts/gfx_shading_check.py` samples shadings against MuPDF's rendering.

## JSON mirror

`rtex dl2json` / `rtex_dl_to_json` convert the binary form to the JSON shape used by
`rtex-dl.lua` and `rtex_dl::DisplayList` (`{"kind","unit","fonts","lines":[{"par","i","x","y","w",
"h","d","gs","gsign","gorder","items":[["g",font,char,index,x,y,w,ef],["r",x,y_top,w,h],
["c",stack,cmd,data],["l",mode,data,x,y],["u",kind,detail],["m","on"|"off",x],["i",index,x,y_top,w,h],
["M","save"|"set"|"restore",x,y,data]]}],
"other":[…],"flags":{…},"pictures":[{"key","x","top","width","height"}],"glyphs":n,"inserts":n,"images_info":{index:{file,page,pages}},"width","height","depth","page","page_width","page_height","origin","rotate"}` (`rotate` only when not 0);
page lines also carry `"unit"` and `"row"`).
Both encodings carry the same information; the binary one is what the engine emits.
