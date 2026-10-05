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
from PIL import Image, ImageOps
import datetime
import io

SCALE = 6  # pixels/mm in the rectified image; a 0.35mm pen is ~2 pixels wide
homography = legacy.homography
tex_escape = legacy.tex_escape


def marker(doc, page):
    return f"CB2:{doc['id']}:{doc['revision']}:{page}"


#: Where every v2 page has its corner targets and its QR (mm from top left);
#: the Rust manifest says the same for each page (a test checks).
V2_FIDUCIALS = [[10,10],[200,10],[200,287],[10,287]]
V2_QR = [187,277]


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
    qx, qy = V2_QR
    box = ((qx-14)*scale, (qy-14)*scale, (qx+14)*scale, (qy+14)*scale)
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
    small = im.copy(); small.thumbnail((1500,2000))
    arr = np.asarray(small); boxes = components(arr < 125)
    candidates = []
    for outer in boxes:
        l,t,r,b,area,_,_ = outer; w,h = r-l+1,b-t+1
        if not (12 <= min(w,h) and max(w,h) <= 85 and .6 < w/h < 1.67 and .25 < area/(w*h) < .9): continue
        for inner in boxes:
            il,it,ir,ib,n,sx,sy = inner
            if not (l < il <= ir < r and t < it <= ib < b and .012 < n/(w*h) < .085): continue
            x,y = sx/n,sy/n
            if abs(x-(l+r)/2) > w*.12 or abs(y-(t+b)/2) > h*.12: continue
            # A clean white moat surrounds the dot. The outer ring remains dark.
            def samples(radius):
                return np.array([arr[int(round(y+math.sin(a)*h*radius)),int(round(x+math.cos(a)*w*radius))]
                                 for a in np.linspace(0,2*math.pi,24,endpoint=False)])
            if np.mean(samples(.21)>180)<.9 or np.mean(samples(.43)<125)<.7: continue
            candidates.append((x*im.width/small.width,y*im.height/small.height))
    if len(candidates) != 4:
        raise ValueError('Show all four corner marks on one flat page')
    return candidates


def normalize(im, found, page):
    if len(found) != 1: raise ValueError('Photograph only one page at a time')
    corners = fiducials(im)
    center = np.mean(corners,axis=0)
    corners.sort(key=lambda p: math.atan2(p[1]-center[1],p[0]-center[0]))
    qr = np.array(found[0][1])
    # The identity QR is close to the bottom-right target, resolving all rotations.
    br = min(range(4),key=lambda i:np.linalg.norm(np.array(corners[i])-qr))
    dst = [corners[(br-2+i)%4] for i in range(4)]
    edges = [np.linalg.norm(np.array(dst[i])-dst[(i+1)%4]) for i in range(4)]
    if min(edges)<500 or max(edges)/min(edges)>3:
        raise ValueError('Photo too small or angle too steep')
    anchors = page['fiducials']
    inverse = np.r_[homography(dst, anchors),1].reshape(3,3)
    q = inverse @ np.r_[qr,1]; q = q[:2]/q[2]
    if np.linalg.norm(q-page['qr_center'])>3:
        raise ValueError('Corner marks and page code do not align')
    h = homography([(x*SCALE,y*SCALE) for x,y in anchors],dst)
    return im.transform((210*SCALE,297*SCALE),Image.Transform.PERSPECTIVE,h,
                        Image.Resampling.BICUBIC,fillcolor=255)


#: Largest local shift (pixels, 1px = 1/SCALE mm) searched between where the
#: manifest puts a box and where its printed outline is in the rectified photo.
REGISTRATION = 2
#: Share of positions along each printed box side that must show the line.
OUTLINE = .8
#: Share of positions along each of the four X arms that must show ink.
ARM = .7
#: How far from the centre (mm) each X arm is checked.
ARM_REACH = 1.45
UNCLEAR = 'Unclear mark: use one clear X, not a tick or filled box'
#: Circles (mm) around the crossing of an X: an X passes each at most four
#: times, whatever the pen; a scribble, an extra line, a grid or a circle
#: more often.
RINGS = (.8, 1.1, 1.4, 1.7)


def continuity(mask, ink, axis):
    """Share of positions along a line (rows for axis=1, columns for axis=0)
    that have ink anywhere across the line's band. Independent of pen width,
    unlike a share of the band's area."""
    present = mask.any(axis=axis)
    if not present.any():
        return 0.
    return float((mask & ink).any(axis=axis)[present].mean())


def arm_continuity(arm, ink, dx, dy):
    """Share of distances from the centre (1.5px steps) along one X arm that
    show ink across the arm's band."""
    if not arm.any():
        return 0.
    # 1.5px steps: a thin diagonal line only has a pixel every sqrt(2) px.
    steps = np.floor(np.hypot(dx, dy)[arm]*SCALE/1.5).astype(int)
    hit = ink[arm]
    seen = np.unique(steps)
    inked = np.unique(steps[hit])
    return len(inked)/len(seen)


def ring_pieces(ink, centre, radius):
    """How many separate pieces of ink a circle of `radius` mm around
    `centre` (patch pixels) passes through. One-sample gaps are closed, so a
    thin line isn't counted twice; a fully inked circle is one piece."""
    cx, cy = centre
    angles = np.linspace(0, 2*math.pi, 120, endpoint=False)
    cols = np.round(cx+np.cos(angles)*radius*SCALE).astype(int)
    rows = np.round(cy+np.sin(angles)*radius*SCALE).astype(int)
    h, w = ink.shape
    if cols.min() < 0 or rows.min() < 0 or cols.max() >= w or rows.max() >= h:
        return 0
    hit = ink[rows, cols]
    hit = hit | (np.roll(hit, 1) & np.roll(hit, -1))
    if hit.all():
        return 1
    return int(np.sum(hit & ~np.roll(hit, 1)))


def field_value(arr, field):
    """Three states: blank, two diagonal strokes (X), or reject as ambiguous.

    The printed outline is registered locally first (within REGISTRATION
    pixels of the manifest position, at sub-pixel precision) and must be
    continuous on all four sides — a missing outline is never read as an
    empty box. Then, ignoring the outline: almost no ink is blank; a
    confident X needs ink running along all four diagonal arms (continuity,
    so thin and thick pens count alike), no circle around its crossing
    passing more than four pieces of ink (no extra strokes), and no filled
    box. Ticks, slashes, dots, fills, grids, crossed-out marks and scribbles
    are ambiguous — except a loose scribble with a thick marker, which can
    look like an X at this size (the preview before applying catches it).
    """
    x,y = field['x']*SCALE,field['y']*SCALE
    half = field['size']/2
    radius = int((half+1.25)*SCALE)+REGISTRATION
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
            ring=(square>half+.55)&(square<half+1.15)
            white=np.median(patch[ring])
            ink=patch < white*.70
            along=abs(yy)<half-.3, abs(xx)<half-.3
            sides=[continuity((abs(xx-half)<.3)&along[0],ink,1),
                   continuity((abs(xx+half)<.3)&along[0],ink,1),
                   continuity((abs(yy-half)<.3)&along[1],ink,0),
                   continuity((abs(yy+half)<.3)&along[1],ink,0)]
            score=(min(sides),-abs(ox)-abs(oy))
            if best is None or score>best[0]:
                best=(score,xx,yy,square,ring,white,ink,(ox,oy))
    (outline,_),xx,yy,square,ring,white,ink,(rx,ry)=best
    centre=(radius+(x-cx)+rx, radius+(y-cy)+ry)
    background=patch[ring]
    if white<120 or np.std(background)>32:
        raise ValueError('Shadow or stray ink near a box; retake the photo')
    if outline<OUTLINE:
        raise ValueError('Box outline unclear; flatten the page and retake')
    inside=square < half-.55
    density=np.mean(ink[inside])
    if density < .018: return False
    if density > .55:
        raise ValueError(UNCLEAR)
    # An ordinary hand-drawn X can be offset and uneven. Search small offsets
    # and slopes for two complete diagonal strokes ...
    for ox in (-.5,-.25,0,.25,.5):
        for oy in (-.5,-.25,0,.25,.5):
            dx,dy=xx-ox,yy-oy
            for slope in (.75,1.,1.3):
                down=abs(dy-slope*dx)<.43   # "\\" in image coordinates
                up=abs(dy+slope*dx)<.43     # "/"
                # Each arm from 0.45 to 1.45mm out: any X using a good half
                # of the box reaches that far; a dot, tick or slash doesn't.
                near=np.hypot(dx,dy)<=ARM_REACH
                arms=[(sx*dx>.3)&(sy*dy>.3)&band&inside&near
                      for sx,sy,band in ((1,1,down),(-1,-1,down),(1,-1,up),(-1,1,up))]
                if min(arm_continuity(a,ink,dx,dy) for a in arms)<ARM: continue
                # ... and nothing else: around the crossing, ink may cross each
                # circle only where the X's four arms do.
                crossing=(centre[0]+ox*SCALE,centre[1]+oy*SCALE)
                if all(ring_pieces(ink,crossing,r)<=4 for r in RINGS):
                    return True
    raise ValueError(UNCLEAR)


def read_marks(im,page):
    marks=[]; arr=np.asarray(im,dtype=float)
    for row in page['rows']:
        selected=[f['kind'] for f in row['fields'] if field_value(arr,f)]
        if not selected: continue
        if len(selected)!=1: raise ValueError('Mark exactly one day with an X per duty')
        chosen=selected[0]
        marks.append({'row':row['id'],'skipped':chosen=='skip','day':None if chosen=='skip' else chosen})
    return marks


def scan(data,doc=None):
    if doc is not None and doc.get('layout_version',1)==1: return legacy.scan(data,doc)
    try:
        with Image.open(io.BytesIO(data)) as source:
            if source.width*source.height>24_000_000: return {}
            im=ImageOps.exif_transpose(source).convert('L')
        im.thumbnail((2600,3600)); found=decode(im)
    except (OSError,ValueError,Image.DecompressionBombError): return {}
    if not found: return legacy.scan(data) if doc is None else {}
    parts=found[0][0]
    result={'document':parts[1],'revision':parts[2],'page':int(parts[3])}
    if doc is None: return result
    try:
        if doc.get('layout_version')!=2: raise ValueError('Unsupported paper layout; request a fresh PDF')
        if parts[1]!=doc['id'] or parts[2]!=doc['revision']: raise ValueError('Unknown document revision')
        page=doc['pages'][int(parts[3])]
        result['marks']=read_marks(normalize(im,found,page),page)
    except (ValueError,IndexError,OverflowError,np.linalg.LinAlgError) as e: result['error']=str(e)
    return result


def span(start, end):
    """'28 Sep', '28–29 Sep', '28 Sep – 4 Oct', '29 Dec 2026 – 4 Jan 2027'."""
    if start == end:
        return f"{start.day} {start:%b}"
    if (start.year, start.month) == (end.year, end.month):
        return f"{start.day}–{end.day} {end:%b}"
    if start.year == end.year:
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
    text = span(start, end)
    return text if start.year != end.year else f"{text} {end.year}"


#: Day columns: the centre of Monday's box and the distance between days
#: (mm). The manifest puts the boxes there; the headers follow them.
DAY_X, DAY_STEP = 111.5, 13


def render(doc, engine='tectonic'):
    if doc.get('layout_version', 1) == 1:
        return legacy.render(doc, engine)
    tex = [r'\documentclass[a4paper]{article}', r'\usepackage[margin=0mm]{geometry}',
           r'\usepackage{graphicx,tikz,fontawesome5}', r'\usepackage[T1]{fontenc}',
           r'\usepackage[utf8]{inputenc}', r'\usepackage{lmodern,helvet}',
           r'\renewcommand{\familydefault}{\sfdefault}', r'\pagestyle{empty}',
           r'\definecolor{accent}{HTML}{2F6F73}', r'\definecolor{ink}{HTML}{253238}',
           r'\definecolor{muted}{HTML}{6B7785}',
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
        bottom = rows[-1]['y']+6.25 if rows else 60
        # Weekend columns, lightly shaded behind everything (never under a
        # box's surroundings the scanner reads: those stay within 3.6mm).
        for day in (5, 6):
            cx = DAY_X+day*DAY_STEP
            tex.append(fr'\fill[accent!7] ({cx-DAY_STEP/2},44) rectangle ({cx+DAY_STEP/2},{bottom});')

        for x, y in page['fiducials']:
            tex.append(fr'\fill ({x-2.5},{y-2.5}) rectangle ({x+2.5},{y+2.5});\fill[white] ({x},{y}) circle (1.5mm);\fill ({x},{y}) circle (.5mm);')
        name = f"qr-{page['number']}.png"
        subprocess.run(['qrencode', '-l', 'M', '-s', '8', '-m', '4', '-o', name, marker(doc, page['number'])],
                       check=True, stdout=subprocess.DEVNULL)
        x, y = page['qr_center']
        tex.append(fr'\node[inner sep=0] at ({x},{y}) {{\includegraphics[width=18mm]{{{name}}}}};')

        # Header: what this is, whose, when, where.
        text(15, 12.5, r'\textbf{CLEANING PLAN}', 7.5, 60, True, color='accent')
        text(15, 17, r'\color{accent}\faBroom\enspace\textbf{'+tex_escape(page['title'][:55])+'}', 21, 150, True)
        text(15, 27.5, 'A little teamwork. A lovely clean home.', 10, 172, color='muted')
        where = r'\faCalendar\enspace '+tex_escape(period(rows)) if rows else ''
        if page.get('rooms'):
            where += r'\qquad\faMapMarker*\enspace '+tex_escape(page['rooms'][:110])
        text(15, 34, where, 9, 175, True)

        tex.append(r'\draw[accent,line width=.6mm] (14,44) -- (196,44);')
        text(20.5, 49, 'Week', 8, 12, anchor='center', align='center', color='muted')
        text(30, 49, 'When' + (' · task' if any(r.get('task') for r in rows) else ''), 8, 45, anchor='west', color='muted')
        text(79, 49, 'Who', 8, 26, anchor='west', color='muted')
        for j, label in enumerate(['Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat', 'Sun']):
            text(DAY_X+j*DAY_STEP, 49, label, 7.5, 9, anchor='center', align='center', color='muted')

        # Rows, grouped by week: a strong rule and one badge per week.
        groups = []
        for row in rows:
            key = (row['year'], row['week'])
            if groups and groups[-1][0] == key:
                groups[-1][1].append(row)
            else:
                groups.append((key, [row]))
        for (_, week), members in groups:
            top = members[0]['y']-6.25
            tex.append(fr'\draw[black!55,line width=.35mm] (14,{top}) -- (196,{top});')
            centre = (members[0]['y']+members[-1]['y'])/2
            tex.append(fr"\node[circle,draw=accent,line width=.3mm,inner sep=0,minimum size=6mm,"
                       fr"font=\small\bfseries,text=accent] at (20.5,{centre}) {{{week}}};")
            for row in members[1:]:
                tex.append(fr'\draw[black!40,line width=.15mm,dotted] (27,{row["y"]-6.25}) -- (196,{row["y"]-6.25});')
        for row in rows:
            y = row['y']
            start = datetime.date.fromisoformat(row['start'])
            end = datetime.date.fromisoformat(row['end'])
            note = ''
            if 'imported' in row.get('label', ''):
                note = ' · imported'
            elif 'assigned' in row.get('label', ''):
                note = ' · assigned'
            when = tex_escape(window(start, end))
            if row.get('task'):
                text(30, y-2.2, r'\textbf{'+tex_escape(row['task'][:34])+'}', 9, 46, True, anchor='west')
                text(30, y+2.4, when+r'{\color{muted}'+tex_escape(note)+'}', 8, 46, True, anchor='west')
            else:
                text(30, y, when+r'{\color{muted}\scriptsize'+tex_escape(note)+'}', 9, 46, True, anchor='west')
            # Up to two lines; the full name stays in the manifest.
            text(79, y, tex_escape(row['name'][:44]), 9.5 if len(row['name']) <= 20 else 8.5, 25, True, anchor='west')
            if row['status']:
                text(109, y, row['status'], 9, 85, anchor='west', color='muted')
            for field in row['fields']:
                fx, fy, half = field['x'], field['y'], field['size']/2
                tex.append(fr'\draw[black,line width=.22mm,fill=white] ({fx-half},{fy-half}) rectangle ({fx+half},{fy+half});')
        tex.append(fr'\draw[accent,line width=.5mm] (14,{bottom}) -- (196,{bottom});')
        for x in (14, 27, 76, 105, 196):
            tex.append(fr'\draw[black!35,line width=.15mm] ({x},44) -- ({x},{bottom});')

        # How to use it, with a little example box.
        ex, ey = 19, 263.2
        tex.append(fr'\draw[black,line width=.22mm] ({ex-2},{ey-2}) rectangle ({ex+2},{ey+2});')
        tex.append(fr'\draw[ink,line width=.35mm,line cap=round] ({ex-1.3},{ey-1.3}) -- ({ex+1.3},{ey+1.3}) ({ex-1.3},{ey+1.3}) -- ({ex+1.3},{ey-1.3});')
        text(23.5, ey, r'\textbf{Done? Put one clear X in the day you cleaned.} Leave the rest blank.', 8.5, 150, True, anchor='west')
        text(15, 281, r'\color{accent}\faHeart\enspace Thanks for keeping our home lovely.', 8, 120, True, anchor='west')
        text(148, 281, f"{page['number']+1} / {len(doc['pages'])}", 8, 20, anchor='west', color='muted')
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
