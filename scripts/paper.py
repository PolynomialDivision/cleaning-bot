"""Paper v2: a wall plan with X boxes, one QR and four small registration targets.
All field positions come from the persisted Rust manifest. No OCR or PDF widgets.
"""
import json
import math
import subprocess
import sys
from pathlib import Path
import paper_v1 as legacy  # Also retains decoding of already printed v1 sheets.
import numpy as np
from PIL import Image, ImageFilter, ImageOps
import datetime
import io
import itertools

SCALE = 6  # pixels/mm in the rectified image; a 0.35mm pen is ~2 pixels wide
homography = legacy.homography

#: Characters TeX must not see raw. The sheet is compiled by Tectonic
#: (XeTeX) in production and pdflatex in tests; with T1-encoded fonts, XeTeX
#: prints a raw "·" as "ů" and drops a raw "–". So the source stays ASCII —
#: the same table as `tex_esc` in src/pdf.rs. Other characters beyond
#: Latin-1 have no glyph in these fonts and are left out.
TEX = {'\\': r'\textbackslash{}', '&': r'\&', '%': r'\%', '$': r'\$', '#': r'\#',
       '_': r'\_', '{': r'\{', '}': r'\}', '~': r'\textasciitilde{}',
       '^': r'\textasciicircum{}', '\u2013': '--', '\u2014': '---',
       '\u00d7': r'$\times$', '\u00f7': r'$\div$', '\u00df': r'{\ss}', '\u00ff': r'\"y',
       '\u00a0': '~', '\u00a1': r'\textexclamdown{}', '\u00a3': r'\pounds{}',
       '\u00a7': r'\S{}', '\u00a9': r'\textcopyright{}', '\u00ab': r'\guillemotleft{}',
       '\u00bb': r'\guillemotright{}', '\u00b0': r'\textdegree{}',
       '\u00b7': r'\textperiodcentered{}', '\u00bf': r'\textquestiondown{}',
       '\u20ac': r'\texteuro{}', '\u2026': r'\dots{}', '\u2018': '`', '\u2019': "'",
       '\u201a': r'\quotesinglbase{}', '\u201c': '``', '\u201d': "''",
       '\u201e': r'\quotedblbase{}'}
#: Latin-1 letters, written as TeX accents so the source stays ASCII.
ACCENTS = {'\u0300': '`', '\u0301': "'", '\u0302': '^', '\u0303': '~', '\u0308': '"',
           '\u030a': 'r', '\u0327': 'c'}
SPECIAL_LETTERS = {'\u00c6': r'\AE{}', '\u00e6': r'\ae{}', '\u00d8': r'\O{}', '\u00f8': r'\o{}',
                   '\u00d0': r'\DH{}', '\u00f0': r'\dh{}', '\u00de': r'\TH{}', '\u00fe': r'\th{}'}


def tex_escape(s):
    import unicodedata
    out = []
    for c in s:
        if c in TEX:
            out.append(TEX[c])
        elif c in SPECIAL_LETTERS:
            out.append(SPECIAL_LETTERS[c])
        elif ord(c) < 128:
            out.append(c)
        elif ord(c) < 256:
            base, *marks = unicodedata.normalize('NFD', c)
            if len(marks) == 1 and marks[0] in ACCENTS and ord(base) < 128:
                accent = ACCENTS[marks[0]]
                out.append(f'\\{accent}{{{base}}}' if accent.isalpha() else f'{{\\{accent}{base}}}')
        # Anything else (emoji, other scripts) has no glyph here: left out.
    return ' '.join(''.join(out).split()) if s.strip() else ''


#: Room kinds drawn as a symbol in the header.
ROOM_ICONS = {'toilet': r'\faToilet', 'shower': r'\faShower', 'kitchen': r'\faUtensils'}


def rooms_tex(page):
    """The header's rooms: per slot, "Colbe: [toilet] 3rd · [toilet] 4th",
    "Scharni: [toilet] [shower]" — a symbol instead of the word for the
    room's kind. A slot is never broken across lines. Manifests from before
    room groups show their plain list."""
    groups = page.get('room_groups') or []
    if not groups:
        return tex_escape(page.get('rooms', '')[:140])
    out = []
    for group in groups:
        body, bare = '', False
        for room in group['rooms']:
            icon = ROOM_ICONS.get(room['kind'])
            label = tex_escape(room['label'])
            item = (r'{\color{accent}'+icon+'}'+(r'\,'+label if label else '')) if icon else label
            if not item:
                continue
            # Bare symbols side by side; anything with words gets a dot.
            if body:
                body += r'\enspace ' if bare and icon and not label else r' \textperiodcentered{} '
            body += item
            bare = bool(icon) and not label
        if not body:
            continue
        slot = r'\textbf{'+tex_escape(group['slot'])+r':}~' if group.get('slot') else ''
        out.append(r'\mbox{'+slot+body+'}')
    return r'\hspace{5mm} '.join(out)


def fit(width, text):
    """`text` (TeX) scaled down to `width` mm if it is wider — a long name
    or word must never run over a column line."""
    return fr'\fit{{{width}mm}}{{{text}}}'


def fit_words(width, text):
    """`text` (plain) for a narrow column: lines wrap between words and
    after a hyphen ("Schwarzenberger-|Lüdenscheidt"); a single part too
    wide for the column is scaled down to fit."""
    words = []
    for word in text.split():
        parts = [p for p in word.replace('-', '-\n').split('\n') if tex_escape(p)]
        if parts:
            # \hspace{0pt}: a place to break, no space.
            words.append(r'\hspace{0pt}'.join(fit(width, tex_escape(p)) for p in parts))
    return ' '.join(words)


def short_name(name, limit=60):
    """`name`, cut after a whole word with "…" if longer than `limit`."""
    name = ' '.join(name.split())
    if len(name) <= limit:
        return name
    cut = name[:limit].rsplit(' ', 1)[0]
    return cut + '…'


def name_size(name):
    """Font size for a name in the Who column (24mm): one line up to ~18
    characters, two up to ~34, else three smaller ones — a row has room."""
    return 9.5 if len(name) <= 18 else 8 if len(name) <= 34 else 7


qr_tikz = legacy.qr_tikz


def marker(doc, page):
    """The page's QR payload. Upper case: hex digits and ":" then fit QR's
    compact alphanumeric mode (29 instead of 33 modules: bigger modules at
    the same size). Read back case-insensitively, as earlier sheets used
    lower case."""
    return f"CB2:{doc['id']}:{doc['revision']}:{page}".upper()


#: Where every v2 page has its corner targets and its QR (mm from top left);
#: the Rust manifest says the same for each page (a test checks).
V2_FIDUCIALS = [[10,10],[200,10],[200,287],[10,287]]
#: Where a v2 page's QR code may be (mm: left, top, right, bottom): around
#: (185,276), 20mm, and on sheets printed earlier (187,277), 18mm.
V2_QR_AREA = (170, 260, 197, 289)
#: Where a new sheet's QR code is centred (mm).
V2_QR_CENTER = (185, 276)
#: Size of the QR code drawn on new sheets (mm, with its quiet zone).
QR_SIZE = 20


def apply(h, point):
    """Map `point` with 8 projective coefficients from `homography`."""
    a,b,c,d,e,f,g,k = h
    x,y = point
    w = g*x+k*y+1
    return [(a*x+b*y+c)/w, (d*x+e*y+f)/w]


def decode(im):
    found = legacy.decode(im, protocol='CB2')
    if not found:
        found = [(parts, [im.width-1-x, im.height-1-y])
                 for parts, (x,y) in legacy.decode(im.transpose(Image.Transpose.ROTATE_180), protocol='CB2')]
    if not found:
        found = decode_rectified(im)
    return found


def decode_rectified(im, scale=8):
    """A small, tilted QR in a low-resolution photo: straighten the page on
    its four corner targets (which don't depend on rotation), try each of the
    four orientations, and read the QR from the straightened, enlarged
    corner. Its position is mapped back into the photo's coordinates."""
    try:
        corners = fiducials(im)
    except ValueError:
        return []
    center = np.mean(corners, axis=0)
    corners.sort(key=lambda p: math.atan2(p[1]-center[1], p[0]-center[0]))
    box = tuple(v*scale for v in V2_QR_AREA)
    for start in range(4):
        dst = [corners[(start+i) % 4] for i in range(4)]
        try:
            h = homography([(x*scale, y*scale) for x, y in V2_FIDUCIALS], dst)
        except np.linalg.LinAlgError:
            continue
        page = im.transform((210*scale, 297*scale), Image.Transform.PERSPECTIVE, h,
                            Image.Resampling.BICUBIC, fillcolor=255)
        crop = page.crop(box)
        for found in (legacy.decode(crop, protocol='CB2'),):
            if len(found) == 1:
                parts, (x, y) = found[0]
                return [(parts, apply(h, (box[0]+x, box[1]+y)))]
    return []


def components(binary):
    """Run-length connected components, avoiding a per-pixel Python flood fill."""
    parents, runs, previous = [], [], []
    def root(i):
        while parents[i] != i:
            parents[i] = parents[parents[i]]
            i = parents[i]
        return i
    for y, row in enumerate(binary):
        edges = np.flatnonzero(np.diff(np.r_[False, row, False]))
        current = []; j = 0
        for left, end in zip(edges[::2], edges[1::2]):
            right = int(end)-1; left = int(left)
            i = len(parents); parents.append(i); runs.append((left,right,y))
            while j < len(previous) and previous[j][1] < left-1: j += 1
            k = j
            while k < len(previous) and previous[k][0] <= right+1:
                a,b = root(i),root(previous[k][2])
                if a != b: parents[a] = b
                k += 1
            current.append((left,right,i))
        previous = current
    boxes = {}
    for i,(left,right,y) in enumerate(runs):
        n = right-left+1; k = root(i)
        if k not in boxes: boxes[k] = [left,y,right,y,0,0.,0.]
        b = boxes[k]
        b[0]=min(b[0],left);b[1]=min(b[1],y);b[2]=max(b[2],right);b[3]=y
        b[4]+=n;b[5]+=(left+right)*n/2;b[6]+=y*n
    return list(boxes.values())


def fiducials(im):
    """Find a black square / white disc / black centre target, independent of rotation.
    The small central dot determines the projective registration point. QR finder
    patterns have a much larger nested centre and do not meet these ratios.
    """
    candidates = fiducial_candidates(im)
    if len(candidates) != 4:
        raise ValueError('Show all four corner marks on one flat page')
    return candidates


#: Corner targets are looked for at these levels, strictest first: (dark,
#: white disc, share of the disc that must be white, share of the ring that
#: must be dark). A small, compressed or dim photo blurs a 5mm target's white
#: disc and centre dot; the looser levels still demand the nested shape.
TARGET_LEVELS = ((125, 180, .9, .7), (150, 190, .8, .6), (175, 200, .75, .6))


def fiducial_candidates(im):
    """Every corner target that can be seen, however many: in the photo and
    with its light evened out, at each of TARGET_LEVELS."""
    found = []
    near = max(8, max(im.size)/200)
    for source in (im, flatten(im)):
        for level in TARGET_LEVELS:
            for point in targets(source, *level):
                if all(math.dist(point, other) > near for other in found):
                    found.append(point)
    return found


def targets(im, dark, light, white_share, ring_share):
    """Corner targets at one level (see TARGET_LEVELS)."""
    small = im.copy(); small.thumbnail((1500,2000))
    arr = np.asarray(small); boxes = components(arr < dark)
    candidates = []
    for outer in boxes:
        l,t,r,b,area,_,_ = outer; w,h = r-l+1,b-t+1
        # Square, or squashed up to 2.5:1 where the photo looks at the page
        # from a slant (the far targets of a sheet lying on a table).
        if not (8 <= min(w,h) and 12 <= max(w,h) <= 85 and .4 < w/h < 2.5 and .25 < area/(w*h) < .9): continue
        for inner in boxes:
            il,it,ir,ib,n,sx,sy = inner
            if not (l < il <= ir < r and t < it <= ib < b and .012 < n/(w*h) < .085): continue
            x,y = sx/n,sy/n
            if abs(x-(l+r)/2) > w*.12 or abs(y-(t+b)/2) > h*.12: continue
            # A clean white moat surrounds the dot. The outer ring remains dark.
            def samples(radius):
                return np.array([arr[int(round(y+math.sin(a)*h*radius)),int(round(x+math.cos(a)*w*radius))]
                                 for a in np.linspace(0,2*math.pi,24,endpoint=False)])
            if np.mean(samples(.21)>light)<white_share or np.mean(samples(.43)<dark)<ring_share: continue
            candidates.append((x*im.width/small.width,y*im.height/small.height))
    return candidates


def page_corners(im, qr):
    """The page's four corner targets in the photo, in V2_FIDUCIALS' order,
    found with the QR code's centre `qr` (which lies at a known place near
    the bottom-right one): of more than four targets — a neighbouring sheet
    in the photo — the four that make a page with it; of three — one in a
    shadow — the fourth where the three and the QR code put it."""
    found = sorted(fiducial_candidates(im), key=lambda p: math.dist(p, qr))[:8]
    qr_mm = tuple(V2_QR_CENTER)
    best = None
    for four in itertools.combinations(found, 4):
        centre = np.mean(four, axis=0)
        ring = sorted(four, key=lambda p: math.atan2(p[1]-centre[1], p[0]-centre[0]))
        for start in range(4):
            dst = [ring[(start+i) % 4] for i in range(4)]
            try:
                to_mm = np.r_[homography(dst, V2_FIDUCIALS), 1].reshape(3, 3)
            except np.linalg.LinAlgError:
                continue
            q = to_mm @ np.r_[qr, 1]; off = math.dist(q[:2]/q[2], qr_mm)
            if off < 6 and plausible(dst) and (best is None or off < best[0]):
                best = (off, dst)
    if best:
        return best[1]
    # Three targets and the QR code: the fourth follows.
    for three in itertools.combinations(found, 3):
        for missing in range(4):
            known = [i for i in range(4) if i != missing]
            for order in itertools.permutations(three):
                try:
                    to_photo = np.r_[homography([V2_FIDUCIALS[i] for i in known]+[qr_mm], list(order)+[qr]), 1].reshape(3, 3)
                except np.linalg.LinAlgError:
                    continue
                v = to_photo @ np.r_[V2_FIDUCIALS[missing], 1]
                dst = [None]*4
                for i, point in zip(known, order): dst[i] = point
                dst[missing] = tuple(v[:2]/v[2])
                if plausible(dst):
                    score = max(abs(angle-90) for angle in corner_angles(dst))
                    if best is None or score < best[0]:
                        best = (score, dst)
    if best is None:
        raise ValueError('Show all four corner marks on one flat page')
    return best[1]


def corner_angles(quad):
    """The inside angles (degrees) of a quadrilateral."""
    angles = []
    for i in range(4):
        a, b, c = np.array(quad[i-1]), np.array(quad[i]), np.array(quad[(i+1) % 4])
        u, v = a-b, c-b
        angles.append(math.degrees(math.acos(np.clip(u@v/(np.linalg.norm(u)*np.linalg.norm(v)), -1, 1))))
    return angles


def plausible(quad):
    """A photographed page: the corners clockwise as printed (not mirrored),
    a convex shape, no angle further than 40 degrees from square, opposite
    sides of similar length."""
    def turn(a, b, c):
        return (b[0]-a[0])*(c[1]-b[1])-(b[1]-a[1])*(c[0]-b[0])
    turns = [turn(quad[i], quad[(i+1) % 4], quad[(i+2) % 4]) for i in range(4)]
    if not all(t > 0 for t in turns):
        return False
    if any(abs(angle-90) > 40 for angle in corner_angles(quad)):
        return False
    sides = [math.dist(quad[i], quad[(i+1) % 4]) for i in range(4)]
    return max(sides[0], sides[2]) < 1.8*min(sides[0], sides[2]) and max(sides[1], sides[3]) < 1.8*min(sides[1], sides[3])


def flatten(im):
    """Even out uneven light — a phone's shadow over a corner, a lamp on one
    side: divide each pixel by the paper's brightness around it. Without it,
    paper in a shadow is darker than the fixed thresholds' "dark" and the QR
    code and a corner target vanish. The neighbourhood (3% of the photo's
    long side) is wider than any solid mark on the page, even when the page
    fills the photo (a corner target is 5mm of 210mm), so ink never counts as
    paper; smoothing it well keeps the paper's brightness across a shadow's
    edge (checked on a real photo, a shadow over the QR corner)."""
    small = im.resize((max(1, im.width//4), max(1, im.height//4)), Image.Resampling.BOX)
    window = max(3, int(max(small.size)*.03)) | 1
    paper = small.filter(ImageFilter.MaxFilter(window)).filter(ImageFilter.GaussianBlur(window*.75))
    paper = np.maximum(np.asarray(paper.resize(im.size, Image.Resampling.BILINEAR), dtype=float), 1)
    return Image.fromarray(np.clip(np.asarray(im, dtype=float)/paper*235, 0, 255).astype(np.uint8))


def normalize(im, found, page, *more):
    """`im` straightened to A4 at SCALE pixels/mm by the page's corner
    targets (and `more` images of the same photo, the same way)."""
    if len(found) != 1: raise ValueError('Photograph only one page at a time')
    qr = np.array(found[0][1])
    # The identity QR is close to the bottom-right target, resolving all
    # rotations (and which targets belong to this page).
    dst = page_corners(im, qr)
    edges = [np.linalg.norm(np.array(dst[i])-dst[(i+1)%4]) for i in range(4)]
    if min(edges)<500 or max(edges)/min(edges)>3:
        raise ValueError('Photo too small or angle too steep')
    anchors = page['fiducials']
    inverse = np.r_[homography(dst, anchors),1].reshape(3,3)
    q = inverse @ np.r_[qr,1]; q = q[:2]/q[2]
    if np.linalg.norm(q-page['qr_center'])>3:
        raise ValueError('Corner marks and page code do not align')
    h = homography([(x*SCALE,y*SCALE) for x,y in anchors],dst)
    straight = [x.transform((210*SCALE,297*SCALE),Image.Transform.PERSPECTIVE,h,
                            Image.Resampling.BICUBIC,fillcolor=255 if x is im else 0)
                for x in (im, *more)]
    return straight[0] if not more else straight


#: Largest local shift (pixels, 1px = 1/SCALE mm) searched between where the
#: manifest puts a box and where its printed outline is in the rectified photo.
REGISTRATION = 2
#: Share of positions along each printed box side that must show the line.
OUTLINE = .8
#: A pixel at most this bright (of the paper around it) belongs to a box's
#: printed outline.
OUTLINE_INK = .9
#: Share of a box's inside (away from its outline) that may show ink and
#: still be blank: JPEG noise, a speck.
BLANK = .008
#: More ink than this, and the box is filled in: a mark taken back. Real
#: marks cover 1.5-17% of a box in photos; a big X with a thick marker up to
#: half of it.
FILLED = .55
#: Scribbled over: ink all across the box (in this share of its 4x4
#: patches, each more than a quarter inked), with at least SCRIBBLED ink.
#: An X leaves the four triangles between its arms free (real ones in
#: photos: at most 38% of the patches, a big one with a thick marker 62%; a
#: scribbled-over box in a photo: 81%).
SPREAD, SCRIBBLED = .75, .2
#: A mark spans at least this share of the box's inside (in either
#: direction); less is a dot or a speck, not a mark.
MARK_SPAN = .4
def outline(size):
    """A box's printed outline (mm), and what is read as its inside (this
    far within the outline) and as the clean paper around it (this band
    outside) — clear of the outline even where a small, compressed photo
    blurs it. Boxes of 6mm and more (new sheets) have a bold 0.4mm outline,
    smaller ones (sheets printed before) a 0.22mm one."""
    if size >= 6:
        return .4, .8, (.8, 1.4)
    return .22, .55, (.55, 1.15)


def continuity(mask, ink, axis):
    """Share of positions along a line (rows for axis=1, columns for axis=0)
    that have ink anywhere across the line's band. Independent of pen width,
    unlike a share of the band's area."""
    present = mask.any(axis=axis)
    if not present.any():
        return 0.
    return float((mask & ink).any(axis=axis)[present].mean())


class Unclear(ValueError):
    """A box that can't be read; says why, for the person to check it."""


def box_state(arr, field):
    """What is in one box: 'blank', 'marked' (any clear mark: an X, a tick,
    a stroke) or 'filled' (scribbled over: taken back), or Unclear.

    The printed outline is registered locally first (within REGISTRATION
    pixels of where `locate` found it, at sub-pixel precision) and must be
    continuous on all four sides, with clean paper around it — a missing
    outline is never read as an empty box. Then, ignoring the outline, the
    share of ink inside decides; a speck too small to be a stroke is
    unclear."""
    x,y = field['x']*SCALE,field['y']*SCALE
    half = field['size']/2
    _, margin, ring_band = outline(field['size'])
    radius = int((half+ring_band[1]+.1)*SCALE)+REGISTRATION
    cx,cy = int(round(x)),int(round(y))
    patch=arr[cy-radius:cy+radius+1,cx-radius:cx+radius+1]
    if patch.shape!=(2*radius+1,2*radius+1):
        raise ValueError('A box is outside the photo; show the whole page')
    py,px=np.mgrid[-radius:radius+1,-radius:radius+1].astype(float)
    best=None
    for oy in range(-REGISTRATION,REGISTRATION+1):
        for ox in range(-REGISTRATION,REGISTRATION+1):
            # Coordinates in mm relative to the (shifted) exact box centre.
            xx=(px-(x-cx)-ox)/SCALE; yy=(py-(y-cy)-oy)/SCALE
            square=np.maximum(abs(xx),abs(yy))
            ring=(square>half+ring_band[0])&(square<half+ring_band[1])
            white=np.median(patch[ring])
            # The printed outline is thin: in a small, compressed photo only
            # a pixel or two of grey. It is found with a lighter threshold
            # than what is drawn in the box.
            line=patch < white*OUTLINE_INK
            along=abs(yy)<half-.3, abs(xx)<half-.3
            sides=[continuity((abs(xx-half)<.3)&along[0],line,1),
                   continuity((abs(xx+half)<.3)&along[0],line,1),
                   continuity((abs(yy-half)<.3)&along[1],line,0),
                   continuity((abs(yy+half)<.3)&along[1],line,0)]
            score=(min(sides),-abs(ox)-abs(oy))
            if best is None or score>best[0]:
                best=(score,square,ring,white)
    (visible,_),square,ring,white=best
    # Clean paper around the box: a stroke running out of it (a big X, a
    # tail) darkens a narrow strip of this ring; a shadow, a fold or
    # writing next to it much of it.
    if white<120 or np.mean(patch[ring] < white*.7) > AROUND:
        raise Unclear('shadow or ink next to the box')
    if visible<OUTLINE:
        raise Unclear('box not clearly visible (fold, blur)')
    inside=square < half-margin
    ink=(patch < white*.70) & inside
    density=float(ink.mean()/inside.mean())
    if density < BLANK: return 'blank'
    if density > FILLED or (density > SCRIBBLED and spread(ink, square, half-margin) >= SPREAD):
        return 'filled'
    ys,xs=np.nonzero(ink)
    if max(np.ptp(xs),np.ptp(ys))/SCALE < (half-margin)*2*MARK_SPAN:
        raise Unclear('only a dot')
    # A mark goes through the middle of the box (an X, a tick, a stroke
    # do); ink only along a side is the printed outline, blurred inwards.
    if not ink[square < half/2].any():
        return 'blank'
    return 'marked'


#: How far a box may lie from where the manifest puts it (mm): a page that
#: isn't flat (curled, wavy, crumpled) moves boxes by up to 3-4mm against
#: the corners. A page bent more (over an edge, say) can move them by half a
#: row and more, so a box may be found in the next row; `locate_all`
#: catches that.
DRIFT = 5
#: Neighbouring boxes found closer together or further apart than this
#: share of their distance on paper: one of them is another row's (or
#: day's) box. (Paper bends smoothly, moving neighbours a millimetre or two
#: against each other; where boxes start to be found in the next row or day,
#: neighbours are suddenly 11-13mm off.)
SPACING = .35


def locate(arr, field, prior=(0., 0.)):
    """`field` moved to where its box outline actually is, within DRIFT of
    `prior` (mm, from where the manifest puts it): where all four sides of a
    box-sized square are darkest (all four, so a row's rule or a column
    line alone never passes for a box). None when no outline stands out."""
    half = field['size']/2*SCALE
    reach = int(DRIFT*SCALE)
    x, y = (field['x']+prior[0])*SCALE, (field['y']+prior[1])*SCALE
    cx, cy = int(round(x)), int(round(y))
    edge = int(round(half))
    size = reach+edge+1
    if cy-size < 0 or cx-size < 0:
        return None
    patch = arr[cy-size:cy+size+1, cx-size:cx+size+1]
    if patch.shape != (2*size+1, 2*size+1):
        return None
    ink = patch < np.percentile(patch, 90)*OUTLINE_INK
    # A pixel's tolerance: on a wavy page a side is not one straight row.
    grown = ink.copy()
    grown[1:] |= ink[:-1]; grown[:-1] |= ink[1:]
    grown[:, 1:] |= ink[:, :-1]; grown[:, :-1] |= ink[:, 1:]
    ink = grown
    # Ink along each side of the square, for every shift at once: running
    # sums along rows and columns of the ink mask.
    rows = np.cumsum(np.pad(ink, ((0, 0), (1, 0))), axis=1)
    cols = np.cumsum(np.pad(ink, ((1, 0), (0, 0))), axis=0)
    span = 2*edge+1
    best, shift = 0., (0, 0)
    for dy in range(-reach, reach+1):
        for dx in range(-reach, reach+1):
            l, t = size+dx-edge, size+dy-edge
            r, b = l+span-1, t+span-1
            sides = ((rows[t, r+1]-rows[t, l])/span, (rows[b, r+1]-rows[b, l])/span,
                     (cols[b+1, l]-cols[t, l])/span, (cols[b+1, r]-cols[t, r])/span)
            score = min(sides)-.002*(abs(dx)+abs(dy))
            if score > best:
                best, shift = score, (dx, dy)
    if best < .5:
        return None
    return dict(field, x=field['x']+prior[0]+shift[0]/SCALE, y=field['y']+prior[1]+shift[1]/SCALE)


def locate_all(arr, page):
    """Where each box of `page` actually is: {field id: field moved there,
    or None}. A box can be found in the wrong row (or day) only where the
    page is bent so much that a whole run of boxes slides over by one; then
    two neighbours land on one box or leave one out between them. So in
    each column and each row the distances between neighbouring boxes must
    be as printed; where they aren't, none of that column's or row's boxes
    is trusted."""
    fields = [f for row in page['rows'] for f in row['fields']]
    found = {f['id']: locate(arr, f) for f in fields}
    columns = {}
    for f in fields:
        columns.setdefault(round(f['x'], 1), []).append(f)
    lines = [sorted(c, key=lambda f: f['y']) for c in columns.values()]
    lines += [sorted(row['fields'], key=lambda f: f['x']) for row in page['rows']]
    for line in lines:
        for a, b in zip(line, line[1:]):
            fa, fb = found[a['id']], found[b['id']]
            if fa is None or fb is None:
                continue
            printed = (b['x']-a['x'], b['y']-a['y'])
            seen = (fb['x']-fa['x'], fb['y']-fa['y'])
            if math.dist(printed, seen) > SPACING*math.hypot(*printed):
                for f in line:
                    found[f['id']] = None
                break
    return found


def spread(ink, square, edge):
    """Share of the box inside's 4x4 patches more than a quarter inked."""
    rows, cols = np.nonzero(square < edge)
    top, left = rows.min(), cols.min()
    height, width = rows.max()-top+1, cols.max()-left+1
    inked = 0
    for i in range(4):
        for j in range(4):
            cell = ink[top+i*height//4:top+(i+1)*height//4, left+j*width//4:left+(j+1)*width//4]
            inked += cell.size > 0 and cell.mean() > .25
    return inked/16


#: Largest share of the paper around a box that may be dark.
AROUND = .2


#: More unclear boxes than this share of a page, and the photo itself is
#: the problem (crumpled, blurred, in shadow): retake it rather than list them.
TOO_UNCLEAR = .25


def read_marks(im, page):
    """The page's marks, the duties that can't be read (with why), and the
    boxes filled in (taken back: not counted, but said, in case it was a
    thick X). A day sheet's row may have one marked day. A row with an
    unclear box, or several marked days, is unclear as a whole: the person
    records it in Matrix instead."""
    marks, unclear, taken_back = [], [], []
    arr = np.asarray(im, dtype=float)
    where = locate_all(arr, page)
    boxes = sum(len(row['fields']) for row in page['rows'])
    bad = 0
    for row in page['rows']:
        marked, why = [], None
        for field in row['fields']:
            try:
                if where[field['id']] is None:
                    raise Unclear('box not clearly visible (fold, blur)')
                state = box_state(arr, where[field['id']])
            except Unclear as e:
                bad += 1; why = why or str(e); continue
            if state == 'marked':
                marked.append(field['kind'])
            elif state == 'filled':
                taken_back.append({'row': row['id'], 'day': None if field['kind'] == 'done' else field['kind']})
        if why is None and len(marked) > 1:
            why = 'several days marked'
        if why is not None:
            unclear.append({'row': row['id'], 'reason': why})
            continue
        if marked:
            # 'done' (a tick sheet's only box): done, the bot dates it.
            chosen = marked[0]
            marks.append({'row': row['id'], 'skipped': chosen == 'skip',
                          'day': None if chosen in ('skip', 'done') else chosen})
    if boxes and bad > max(3, TOO_UNCLEAR*boxes):
        raise ValueError('Too much of the page is unclear (folds, shadow or blur): '
                         'smooth it out and take the photo again, straight from the front')
    return marks, unclear, taken_back


def scan(data,doc=None):
    if doc is not None and doc.get('layout_version',1)==1: return legacy.scan(data,doc)
    try:
        with Image.open(io.BytesIO(data)) as source:
            if source.width*source.height>24_000_000: return {}
            im=ImageOps.exif_transpose(source).convert('L')
        im.thumbnail((2600,3600)); found=decode(im)
    except (OSError,ValueError,Image.DecompressionBombError): return {}
    # The light evened out (a shadow over a corner or a box decides
    # nothing): the code read from either, the boxes from the evened one.
    flat = flatten(im)
    found = found or decode(flat)
    if not found:
        if doc is not None: return {}
        result = legacy.scan(data)
        # Clearly a sheet (corner targets), but no code to read: say so
        # rather than ignore it like any other picture.
        if not result.get('document') and len(fiducial_candidates(im)) >= 3:
            result = {'unreadable': True}
        return result
    parts=[part.lower() for part in found[0][0]]
    result={'document':parts[1],'revision':parts[2],'page':int(parts[3])}
    if doc is None: return result
    try:
        if doc.get('layout_version')!=2: raise ValueError('Unsupported paper layout; request a fresh PDF')
        if parts[1]!=doc['id'] or parts[2]!=doc['revision']: raise ValueError('Unknown document revision')
        page=doc['pages'][int(parts[3])]
        result['marks'],result['unclear'],result['taken_back']=read_marks(normalize(flat,found,page),page)
    except (ValueError,IndexError,OverflowError,np.linalg.LinAlgError) as e: result['error']=str(e)
    return result


def span(start, end, years=False):
    """'28 Sep', '28–29 Sep', '28 Sep – 4 Oct', '28 Dec – 3 Jan'; with
    `years`, a range across New Year says both: '28 Dec 2026 – 3 Jan 2027'."""
    if start == end:
        return f"{start.day} {start:%b}"
    if (start.year, start.month) == (end.year, end.month):
        return f"{start.day}–{end.day} {end:%b}"
    if start.year == end.year or not years:
        return f"{start.day} {start:%b} – {end.day} {end:%b}"
    return f"{start.day} {start:%b} {start.year} – {end.day} {end:%b} {end.year}"


def window(start, end):
    """When a duty is due: the dates for a whole week (Mon–Sun), else the
    weekdays first — 'Mon–Tue · 28–29 Sep', 'Thu · 1 Oct'."""
    if start.weekday() == 0 and (end-start).days == 6:
        return span(start, end)
    days = f"{start:%a}" if start == end else f"{start:%a}–{end:%a}"
    return f"{days} · {span(start, end)}"


def period(rows):
    """The page's whole range, with the year: '28 Sep – 20 Nov 2026'."""
    start = min(datetime.date.fromisoformat(r['start']) for r in rows)
    end = max(datetime.date.fromisoformat(r['end']) for r in rows)
    text = span(start, end, years=True)
    return text if start.year != end.year else f"{text} {end.year}"


#: Day columns: the centre of Monday's box and the distance between days
#: (mm). The manifest puts the boxes there; the headers follow them.
DAY_X, DAY_STEP = 111.5, 13


def footer(tex, text, doc, page, how):
    """How to use the sheet, with a little example box; thanks; page number."""
    ex, ey = 19, 271
    tex.append(fr'\draw[black,line width=.22mm] ({ex-2},{ey-2}) rectangle ({ex+2},{ey+2});')
    tex.append(fr'\draw[ink,line width=.35mm,line cap=round] ({ex-1.3},{ey-1.3}) -- ({ex+1.3},{ey+1.3}) ({ex-1.3},{ey+1.3}) -- ({ex+1.3},{ey-1.3});')
    text(23.5, ey, how, 8.5, 150, True, anchor='west')
    text(15, 280, r'\color{accent}\faHeart\enspace Thanks for keeping our home lovely.', 8, 120, True, anchor='west')
    text(150, 280, f"{page['number']+1} / {len(doc['pages'])}", 8, 20, anchor='west', color='muted')


def view_footer(tex, text, doc, page):
    """A view's footer: framed, that this is not the sheet to tick."""
    tex.append(r'\draw[accent,line width=.5mm,rounded corners=1.5mm,fill=white] (14,264) rectangle (196,277);')
    text(19, 270.5, r'\color{accent}\faEye', 14, 8, True, anchor='west')
    text(28, 270.5, r'\textbf{Only to look at: this is not the sheet to tick.}\newline '
         r'Mark your duty with an X on the cleaning plan with boxes, or with !done in Matrix.',
         9, 165, True, anchor='west')
    text(15, 282, r'\color{accent}\faHeart\enspace Thanks for keeping our home lovely.', 8, 120, True, anchor='west')
    text(176, 282, f"{page['number']+1} / {len(doc['pages'])}", 8, 20, anchor='west', color='muted')


def days_of(row):
    """Every day `row` may be done on."""
    day = datetime.date.fromisoformat(row['start'])
    end = datetime.date.fromisoformat(row['end'])
    while day <= end:
        yield day
        day += datetime.timedelta(days=1)


#: Tick sheets: the week's dates between the badge and the slot columns (mm).
TICK_WHEN_X, TICK_COLUMNS_X = 27, 52


def render_ticks(tex, text, page, rows, top, first_top, half, bottom):
    """A tick sheet's table: a line per week, the slots (and shifts) side by
    side, in each cell who and one box. Columns and boxes from the manifest."""
    columns = page.get('columns') or []
    for column in columns:
        left, width = column['left'], column['right']-column['left']
        title = tex_escape(column['title'][:40]) or 'Who'
        if column['subtitle']:
            text(left+1.5, first_top-5.3, r'\textbf{'+fit(width-3, title)+'}', 8.5, width-2, True, anchor='west')
            text(left+1.5, first_top-2.1, fit(width-3, tex_escape(column['subtitle'])), 7.5, width-2, True,
                 anchor='west', color='muted')
        else:
            text(left+1.5, first_top-4, r'\textbf{'+fit(width-3, title)+'}', 8.5, width-2, True, anchor='west')
    text(20.5, first_top-4, 'Week', 8, 12, anchor='center', align='center', color='muted')
    text(TICK_WHEN_X+3, first_top-4, 'When', 8, 22, anchor='west', color='muted')

    weeks = []
    for row in rows:
        if weeks and (weeks[-1][0]['year'], weeks[-1][0]['week']) == (row['year'], row['week']):
            weeks[-1].append(row)
        else:
            weeks.append([row])
    for members in weeks:
        y = members[0]['y']
        tex.append(fr'\draw[black,line width=.35mm] (14,{y-half}) -- (196,{y-half});')
        tex.append(fr"\node[circle,draw=accent,line width=.3mm,inner sep=0,minimum size=6mm,"
                   fr"font=\small\bfseries,text=accent] at (20.5,{y}) {{{members[0]['week']}}};")
        start = min(datetime.date.fromisoformat(r['start']) for r in members)
        end = max(datetime.date.fromisoformat(r['end']) for r in members)
        text(TICK_WHEN_X+2.5, y, fit(21.5, tex_escape(span(start, end))), 8.5, 23, True, anchor='west')
        for row in members:
            if row.get('column', 0) >= len(columns):
                continue
            column = columns[row.get('column', 0)]
            left, right = column['left'], column['right']
            # The name stops well before the box: the scanner reads the
            # paper around it (RING), ink there would make the box unclear.
            box = row['fields'][0] if row['fields'] else None
            width = box['x']-box['size']/2-outline(box['size'])[2][1]-1-(left+1.5) if box else right-left-3
            name = short_name(row['name'])
            if row['status']:
                text(left+1.5, y-2, fit_words(width, name), min(name_size(name), 8.5), width+1, True, anchor='west')
                text(left+1.5, y+2.6, fit(right-left-3, tex_escape(row['status'])), 7, right-left-2, True,
                     anchor='west', color='muted')
            else:
                text(left+1.5, y, fit_words(width, name), name_size(name), width+1, True, anchor='west')
            for field in row['fields']:
                fx, fy, h = field['x'], field['y'], field['size']/2
                tex.append(fr'\draw[black,line width={outline(field["size"])[0]}mm,fill=white] ({fx-h},{fy-h}) rectangle ({fx+h},{fy+h});')
    tex.append(fr'\draw[accent,line width=.5mm] (14,{bottom}) -- (196,{bottom});')
    for x in [14, TICK_WHEN_X, TICK_COLUMNS_X]+[c['right'] for c in columns]:
        tex.append(fr'\draw[black,line width=.2mm] ({x},{top}) -- ({x},{bottom});')


def render(doc, engine='tectonic'):
    if doc.get('layout_version', 1) == 1:
        return legacy.render(doc, engine)
    tex = [r'\documentclass[a4paper]{article}', r'\usepackage[margin=0mm]{geometry}',
           r'\usepackage{graphicx,tikz,fontawesome5}', r'\usepackage[T1]{fontenc}',
           r'\usepackage[utf8]{inputenc}', r'\usepackage{lmodern,helvet}',
           r'\renewcommand{\familydefault}{\sfdefault}', r'\pagestyle{empty}',
           # Printer-friendly: everything solid black (DeviceGray 0), no
           # colour and no grey. A cheap or low-toner printer renders a grey
           # or a dark colour as a pale dot screen; dotted grey lines vanish.
           # Lines and text are told apart by weight and size instead.
           r'\definecolor{accent}{gray}{0}', r'\definecolor{ink}{gray}{0}',
           r'\definecolor{muted}{gray}{0}',
           # Scale a box down to #1 if it is wider (names, slot names).
           r'\newcommand{\fit}[2]{\sbox0{#2}\ifdim\wd0>#1\resizebox{#1}{!}{\usebox0}\else\usebox0\fi}',
           r'\begin{document}']
    for page in doc['pages']:
        if page['number']:
            tex.append(r'\newpage')
        tex.append(r'\null\begin{tikzpicture}[remember picture,overlay,x=1mm,y=-1mm,shift={(current page.north west)}]')

        def text(x, y, s, size=10, width=175, raw=False, anchor='north west', color='ink', align='left'):
            value = s if raw else tex_escape(s)
            tex.append(fr'\node[anchor={anchor},inner sep=0,text width={width}mm,align={align},'
                       fr'font=\fontsize{{{size}}}{{{size*1.2:.1f}}}\selectfont,text={color}] at ({x},{y}) {{{value}}};')

        rows = page['rows']
        tick = page.get('style') == 'tick'
        view = bool(doc.get('view_only'))
        # Row height from the manifest (earlier sheets used 12.5mm). On a
        # tick sheet a week's duties share one line.
        ys = sorted({r['y'] for r in rows})
        half = (ys[1]-ys[0])/2 if len(ys) > 1 else 5.5
        first_top = ys[0]-half if rows else 31
        top = first_top-(8 if tick else 6)          # the column titles' row
        bottom = ys[-1]+half if rows else first_top
        # A view has nothing to tick: a solid bar across the days a duty may
        # be done (like a calendar's), nothing that looks like a box.
        for row in rows if view and not tick else []:
            days = list(days_of(row)) if not row.get('status') else []
            if days:
                left = DAY_X+days[0].weekday()*DAY_STEP-DAY_STEP/2+2
                right = DAY_X+days[-1].weekday()*DAY_STEP+DAY_STEP/2-2
                tex.append(fr'\fill[black,rounded corners=1.2mm] ({left},{row["y"]-1.2}) rectangle ({right},{row["y"]+1.2});')

        # The scanner's marks: corner targets and the QR code. A view has
        # neither, so a photo of it is never taken for a sheet.
        if not view:
            for x, y in page['fiducials']:
                tex.append(fr'\fill ({x-2.5},{y-2.5}) rectangle ({x+2.5},{y+2.5});\fill[white] ({x},{y}) circle (1.5mm);\fill ({x},{y}) circle (.5mm);')
            x, y = page['qr_center']
            tex.append(qr_tikz(marker(doc, page['number']), x, y, QR_SIZE))

        # Header in one line: whose plan, then when and where beside it (on
        # two lines, bottom-aligned, if there are many rooms).
        label = r'CLEANING PLAN\enspace\textperiodcentered\enspace VIEW ONLY, NOT FOR TICKING' if view else 'CLEANING PLAN'
        text(15, 8.5, r'\textbf{'+label+'}', 7, 120, True, color='accent')
        where = r'\faCalendar\enspace '+tex_escape(period(rows)) if rows else ''
        rooms = rooms_tex(page)
        if rooms:
            where += r'\hspace{5mm}\mbox{\faMapMarker*\enspace}'+rooms
        title = r'{\color{accent}\faBroom\enspace\textbf{'+fit(110, tex_escape(page['title'][:55]))+'}}'
        tex.append(fr'\node[anchor=base west,inner sep=0,text=ink] at (15,{top-4.5}) '
                   fr'{{\fontsize{{20}}{{24}}\selectfont\sbox1{{{title}}}\usebox1\hspace{{6mm}}'
                   fr'\parbox[b]{{\dimexpr 180mm-\wd1-6mm}}{{\raggedright\fontsize{{9}}{{11}}\selectfont {where}}}}};')

        tex.append(fr'\draw[accent,line width=.6mm] (14,{top}) -- (196,{top});')
        if tick:
            render_ticks(tex, text, page, rows, top, first_top, half, bottom)
            if view:
                view_footer(tex, text, doc, page)
            else:
                footer(tex, text, doc, page, r'\textbf{Done? Put one clear X in your box.} Leave the rest blank.')
            tex.append(r'\end{tikzpicture}')
            continue
        head = first_top-3
        text(20.5, head, 'Week', 8, 12, anchor='center', align='center', color='muted')
        text(30, head, 'When' + (' · task' if any(r.get('task') for r in rows) else ''), 8, 45, anchor='west', color='muted')
        text(79, head, 'Who', 8, 26, anchor='west', color='muted')
        for j, label in enumerate(['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun']):
            text(DAY_X+j*DAY_STEP, head, label, 7.5, 9, anchor='center', align='center', color='muted')

        # Rows, grouped by week: a strong rule and one badge per week.
        groups = []
        for row in rows:
            key = (row['year'], row['week'])
            if groups and groups[-1][0] == key:
                groups[-1][1].append(row)
            else:
                groups.append((key, [row]))
        for (_, week), members in groups:
            rule = members[0]['y']-half
            tex.append(fr'\draw[black,line width=.35mm] (14,{rule}) -- (196,{rule});')
            centre = (members[0]['y']+members[-1]['y'])/2
            tex.append(fr"\node[circle,draw=accent,line width=.3mm,inner sep=0,minimum size=6mm,"
                       fr"font=\small\bfseries,text=accent] at (20.5,{centre}) {{{week}}};")
            for row in members[1:]:
                tex.append(fr'\draw[black,line width=.2mm,dash pattern=on 1mm off 1mm] (27,{row["y"]-half}) -- (196,{row["y"]-half});')
        for row in rows:
            y = row['y']
            start = datetime.date.fromisoformat(row['start'])
            end = datetime.date.fromisoformat(row['end'])
            when = fit(44, tex_escape(window(start, end)))
            if row.get('task'):
                text(30, y-2.2, r'\textbf{'+fit(44, tex_escape(row['task'][:40]))+'}', 9, 46, True, anchor='west')
                text(30, y+2.4, when, 8, 46, True, anchor='west')
            else:
                text(30, y, when, 9, 46, True, anchor='west')
            # Wraps between words (two lines at most fit a row); a word too
            # long for the column is scaled down. The full name stays in the
            # manifest.
            name = short_name(row['name'])
            text(79, y, fit_words(24, name), name_size(name), 25, True, anchor='west')
            if row['status']:
                text(109, y, row['status'], 9, 85, anchor='west', color='muted')
            for field in row['fields']:
                fx, fy, h = field['x'], field['y'], field['size']/2
                tex.append(fr'\draw[black,line width={outline(field["size"])[0]}mm,fill=white] ({fx-h},{fy-h}) rectangle ({fx+h},{fy+h});')
        tex.append(fr'\draw[accent,line width=.5mm] (14,{bottom}) -- (196,{bottom});')
        for x in (14, 27, 76, 105, 196):
            tex.append(fr'\draw[black,line width=.2mm] ({x},{top}) -- ({x},{bottom});')

        if view:
            view_footer(tex, text, doc, page)
        else:
            footer(tex, text, doc, page, r'\textbf{Done? Put one clear X in the day you cleaned.} Leave the rest blank.')
        tex.append(r'\end{tikzpicture}')
    tex.append(r'\end{document}')
    Path('sheet.tex').write_text('\n'.join(tex))
    args = ['tectonic', 'sheet.tex'] if engine == 'tectonic' else ['pdflatex', '-interaction=nonstopmode', '-halt-on-error', 'sheet.tex']
    for _ in range(2 if engine != 'tectonic' else 1):
        proc = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=45)
        if proc.returncode:
            raise RuntimeError(proc.stdout.decode(errors='replace')[-2500:]+proc.stderr.decode(errors='replace')[-2000:])
    return Path('sheet.pdf').read_bytes()


if __name__=='__main__':
    legacy.limit_worker()  # the bot's worker: bounded memory and CPU time
    mode=sys.argv[1];data=Path('input').read_bytes()
    if mode=='pdf':sys.stdout.buffer.write(render(json.loads(data)))
    else: print(json.dumps(scan(data,json.loads(Path('manifest.json').read_text()) if mode=='scan' else None)))
