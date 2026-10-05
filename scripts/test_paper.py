"""Render printed forms, draw simulated pen strokes, photograph and scan back.
Synthetic handwriting tests are regression checks, not real-camera calibration.
"""
import datetime as dt
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from PIL import Image, ImageDraw, ImageFilter, ImageEnhance
import numpy as np
import paper

#: The TeX engine for rendering: pdflatex by default; set PAPER_ENGINE=tectonic
#: to test what production uses (its package cache must be warm).
ENGINE=os.environ.get('PAPER_ENGINE','pdflatex')


def document():
    pages=[]
    for number,(title,mode,count) in enumerate([('Kitchen','twice',20),('Upper Floor','slots',20),('Bathroom','weekly',21)]):
        rows=[]
        for i in range(count):
            week_offset=i//2 if mode!='weekly' else i
            start=dt.date(2026,9,28)+dt.timedelta(weeks=week_offset)
            if mode=='twice': start+=dt.timedelta(days=3*(i%2))
            end=start+dt.timedelta(days=1 if mode=='twice' else 6)
            # Rows as src/paper.rs lays them out: 11-14mm, filling the page.
            y=31+(i+.5)*min(max(231/count,11),14);rowid=f'{number}-{i}'
            fields=[]
            for offset in range((end-start).days+1):
                day=start+dt.timedelta(days=offset)
                fields.append(dict(id=f'{rowid}:{day}',kind=str(day),x=111.5+day.weekday()*13,y=y,size=4.8,label=day.strftime('%a')))
            rows.append(dict(id=rowid,week=start.isocalendar().week,year=start.isocalendar().year,
                             # Real-world names: one long word, long and accented
                             # names, an emoji — none may cross a column line.
                             name=['Alice','Wolkenschieberin','Zoë Maximiliane Schwarzenberger-Lüdenscheidt','Kim 🌸 Straße'][i%4],
                             label='Bathroom imported' if i==3 else '',task=['Stairs','Hallway'][i%2] if mode=='slots' else '',
                             start=str(start),end=str(end),y=y,status='',fields=fields))
        room=lambda kind,label='':dict(kind=kind,label=label)
        groups={'Kitchen':[dict(slot=None,rooms=[room('kitchen'),room('other','Pantry')])],
                'Upper Floor':[dict(slot='Stairs',rooms=[room('toilet','3rd'),room('toilet','4th')]),
                               dict(slot='Hallway',rooms=[room('toilet'),room('shower')])],
                'Bathroom':[dict(slot=None,rooms=[room('shower'),room('toilet'),room('other','Sink')])]}[title]
        pages.append(dict(number=number,title=title,room_groups=groups,rooms={'Kitchen':'Counters · Sink · Floor','Upper Floor':'Stairs · Hallway','Bathroom':'Shower · Toilet · Sink'}[title],
                          rows=rows,fiducials=[[10,10],[200,10],[200,287],[10,287]],qr_center=[185,276]))
    return dict(layout_version=2,id='0123456789abcdef0123456789abcdef',revision='abcdef012345',pages=pages)


def tick_document():
    """A tick sheet as src/paper.rs lays it out: a line per week, the slots
    and shifts side by side from x 52 to 196, one box per duty 4.5mm left of
    its column's right edge."""
    columns=[]
    for i,(title,subtitle) in enumerate([('Stairs','Mon–Tue'),('Hallway','Mon–Tue'),('Stairs','Thu–Fri'),('Hallway','Thu–Fri')]):
        columns.append(dict(title=title,subtitle=subtitle,left=52+i*36,right=52+(i+1)*36))
    names=['Alice','Wolkenschieberin','Zoë Maximiliane Schwarzenberger-Lüdenscheidt','Kim 🌸 Straße','Bo']
    rows=[]
    for week in range(21):
        y=31+(week+.5)*11;monday=dt.date(2026,9,28)+dt.timedelta(weeks=week)
        for column,c in enumerate(columns):
            start=monday+dt.timedelta(days=3 if column>=2 else 0);end=start+dt.timedelta(days=1)
            rowid=f't-{week}-{column}'
            # Week 0 is partly closed: done in one cell, skipped in another.
            status={(0,0):'Done · Mon 28 Sep',(0,1):'Skipped'}.get((week,column),'')
            fields=[] if status else [dict(id=f'{rowid}:done',kind='done',x=c['right']-4.5,y=y,size=4.8,label='Done')]
            rows.append(dict(id=rowid,week=start.isocalendar().week,year=start.isocalendar().year,
                             name=names[(week+column)%len(names)],label='',task=c['title'],column=column,
                             start=str(start),end=str(end),y=y,status=status,fields=fields))
    page=dict(number=0,title='Upper Floor',rooms='Stairs · Hallway',style='tick',columns=columns,
              room_groups=[dict(slot='Stairs',rooms=[dict(kind='toilet',label='3rd')]),
                           dict(slot='Hallway',rooms=[dict(kind='shower',label='')])],
              rows=rows,fiducials=[[10,10],[200,10],[200,287],[10,287]],qr_center=[185,276])
    return dict(layout_version=2,id='fedcba9876543210fedcba9876543210',revision='0123456789ab',pages=[page])


def colours(pdf):
    """Every colour a PDF's pages set: (operator, values) — only DeviceGray
    0 and 1 (solid black, white) for a printer-friendly sheet."""
    import re,zlib
    found=set()
    for m in re.finditer(rb'stream\r?\n(.*?)\r?\nendstream',pdf,re.S):
        try: content=zlib.decompress(m.group(1))
        except zlib.error: continue
        # Page content is text; embedded fonts (binary) are not drawing.
        if sum(b>127 for b in content)>len(content)//20: continue
        for values,op in re.findall(rb'((?:-?[\d.]+\s+){1,4})(rg|RG|g|G|k|K|sc|SC|scn|SCN)\b',content):
            found.add((op.decode(),tuple(float(v) for v in values.split())))
    return found


def encoded(im,jpeg=False):
    b=io.BytesIO();im.save(b,format='JPEG' if jpeg else 'PNG',**({'quality':78} if jpeg else {}));return b.getvalue()


def pen_x(im,field,width=2,offset=(0,0),gray=25,uneven=False,r=9):
    # Two separate strokes, around 0.33mm at 6px/mm. No fill operations.
    x,y=field['x']*6+offset[0],field['y']*6+offset[1]
    draw=ImageDraw.Draw(im)
    draw.line([(x-r,y-r+int(uneven)),(x+1,y),(x+r,y+r-2*int(uneven))],fill=gray,width=width)
    draw.line([(x-r,y+r),(x,y+int(uneven)),(x+r-2*int(uneven),y-r)],fill=gray,width=width)


def raster(page,prefix):
    subprocess.run(['pdftoppm','-f',str(page+1),'-scale-to-x','1260','-scale-to-y','1782','-png','-singlefile','sheet.pdf',prefix],check=True,stdout=subprocess.DEVNULL)
    return Image.open(prefix+'.png').convert('L')


class PaperTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.doc=document();cls.tmp=tempfile.TemporaryDirectory();cls.old=os.getcwd();os.chdir(cls.tmp.name)
        paper.render(cls.doc,engine=ENGINE)
        cls.blanks=[raster(i,f'sheet-{i}') for i in range(3)];cls.blank=cls.blanks[0]
        out=Path(__file__).resolve().parent.parent/'artifacts';out.mkdir(exist_ok=True)
        (out/'paper-example.pdf').write_bytes(Path('sheet.pdf').read_bytes())
        # Colour previews as printed, and the greyscale the scanner works on
        # (also what a black-and-white printer makes of it).
        for i,(name,im) in enumerate(zip(['twice-weekly','multi-slot','weekly'],cls.blanks)):
            subprocess.run(['pdftoppm','-f',str(i+1),'-r','150','-png','-singlefile','sheet.pdf','colour'],check=True,stdout=subprocess.DEVNULL)
            Image.open('colour.png').save(out/f'paper-{name}.png')
            im.save(out/f'paper-{name}-bw.png')
        Image.open(out/'paper-twice-weekly.png').save(out/'paper-example.png')

    @classmethod
    def tearDownClass(cls):os.chdir(cls.old);cls.tmp.cleanup()

    def marked(self,row=0,fields=('2026-09-28',),page=0,**kwargs):
        im=self.blanks[page].copy()
        for f in self.doc['pages'][page]['rows'][row]['fields']:
            if f['kind'] in fields:pen_x(im,f,**kwargs)
        return im

    def test_one_qr_four_small_targets_and_empty_fields(self):
        self.assertEqual(len(paper.decode(self.blank)),1)
        self.assertEqual(len(paper.fiducials(self.blank)),4)
        for im in self.blanks:
            result=paper.scan(encoded(im),self.doc)
            self.assertEqual(result.get('marks'),[],result)

    def test_pen_width_offset_and_uneven_cross(self):
        for width,gray,offset in [(1,30,(0,0)),(2,90,(1,-1)),(3,20,(-1,1)),(2,40,(2,0))]:
            result=paper.scan(encoded(self.marked(width=width,gray=gray,offset=offset,uneven=True)),self.doc)
            self.assertEqual(result.get('marks'),[dict(row='0-0',skipped=False,day='2026-09-28')],
                             (dict(width=width,gray=gray,offset=offset),result))

    def test_second_shift_of_the_week(self):
        result=paper.scan(encoded(self.marked(1,('2026-10-02',))),self.doc)
        self.assertEqual(result.get('marks'),[dict(row='0-1',skipped=False,day='2026-10-02')],result)

    def test_weekly_and_multi_slot_last_day(self):
        for page in [1,2]:
            result=paper.scan(encoded(self.marked(0,('2026-10-04',),page=page)),self.doc)
            self.assertEqual(result.get('marks'),[dict(row=f'{page}-0',skipped=False,day='2026-10-04')],result)

    def test_rotation_perspective_and_jpeg(self):
        src=[(0,0),(1260,0),(1260,1782),(0,1782)]
        dst=[(180,110),(1450,50),(1330,2080),(60,1860)]
        h=paper.homography(dst,src)
        photo=self.marked().transform((1560,2160),Image.Transform.PERSPECTIVE,h,Image.Resampling.BICUBIC,fillcolor=220)
        photo=ImageEnhance.Brightness(photo).enhance(.88)
        for angle in [0,90,180,270,13]:
            result=paper.scan(encoded(photo.rotate(angle,expand=True,fillcolor=220),jpeg=True),self.doc)
            self.assertEqual(result.get('marks'),[dict(row='0-0',skipped=False,day='2026-09-28')],
                             (dict(angle=angle),result))
        out=Path(__file__).resolve().parent.parent/'artifacts'
        photo.save(out/'paper-phone-simulation.jpg')

    def test_several_days_marked_is_unclear(self):
        for fields in [('2026-09-28','2026-09-29'),('2026-09-28','2026-09-29','2026-10-01')]:
            result=paper.scan(encoded(self.marked(fields=fields)),self.doc)
            self.assertEqual((result.get('marks'),result.get('unclear')),
                             ([],[dict(row='0-0',reason='several days marked')]),result)

    def test_any_clear_mark_counts_and_a_filled_box_is_taken_back(self):
        f,g=self.doc['pages'][0]['rows'][0]['fields'][:2];x,y=f['x']*6,f['y']*6
        mon=[dict(row='0-0',skipped=False,day='2026-09-28')]
        shapes={'tick':(lambda d:d.line([(x-8,y),(x-2,y+7),(x+8,y-8)],fill=0,width=2),mon,[]),
                'slash':(lambda d:d.line([(x-8,y+8),(x+8,y-8)],fill=0,width=2),mon,[]),
                'dot':(lambda d:d.ellipse((x-2,y-2,x+2,y+2),fill=0),[],[dict(row='0-0',reason='only a dot')]),
                'fill':(lambda d:d.rectangle((x-9,y-9,x+9,y+9),fill=0),[],[])}
        for name,(draw,marks,unclear) in shapes.items():
            im=self.blank.copy();draw(ImageDraw.Draw(im))
            result=paper.scan(encoded(im),self.doc)
            self.assertEqual((result.get('marks'),result.get('unclear')),(marks,unclear),(name,result))
        # A filled box is said (taken back), with its day.
        self.assertEqual(result.get('taken_back'),[dict(row='0-0',day='2026-09-28')])
        # Taken back and marked again: Monday filled in, an X on Tuesday.
        im=self.blank.copy();ImageDraw.Draw(im).rectangle((x-9,y-9,x+9,y+9),fill=0);pen_x(im,g)
        result=paper.scan(encoded(im),self.doc)
        self.assertEqual(result.get('marks'),[dict(row='0-0',skipped=False,day='2026-09-29')],result)

    def test_missing_marker_partial_blur_unrelated(self):
        self.assertEqual(paper.scan(encoded(Image.new('L',(1260,1782),200)),self.doc),{})
        # A corner target lost (in a shadow, say): three and the QR code do.
        im=self.blank.copy();ImageDraw.Draw(im).rectangle((40,40,85,85),fill=255)
        self.assertEqual(paper.scan(encoded(im),self.doc).get('marks'),[])
        self.assertIn('error',paper.scan(encoded(self.blank.crop((0,150,1260,1782))),self.doc))
        self.assertFalse(paper.scan(encoded(self.marked().filter(ImageFilter.GaussianBlur(5))),self.doc).get('marks'))

    def test_box_states(self):
        """Directly on the 6px/mm raster (the scanner's own coordinate
        system), so many variants stay fast: every X (pen 0.17-0.67mm, grey
        to black, off-centre, small to large, uneven) and every other clear
        mark counts; a dot is unclear; a filled box is taken back."""
        field=self.doc['pages'][0]['rows'][0]['fields'][0]
        x,y=field['x']*6,field['y']*6
        def state(draw):
            im=self.blank.copy();draw(ImageDraw.Draw(im),im)
            try: return paper.box_state(np.asarray(im,dtype=float),field)
            except paper.Unclear as e: return f'unclear: {e}'
        self.assertEqual(state(lambda d,im:None),'blank')
        # (A 0.67mm marker's X can fill an old 4.8mm box: it then reads as
        # filled, which the preview names; new sheets have 6mm boxes.)
        for width in (1,2,3):
            for gray in (10,40,90):
                for offset in ((0,0),(2,-1),(-2,2),(1,2)):
                    for r in (7,9,11):
                        for uneven in (False,True):
                            case=dict(width=width,gray=gray,offset=offset,r=r,uneven=uneven)
                            self.assertEqual(state(lambda d,im:pen_x(im,field,width,offset,gray,uneven,r)),'marked',case)
        shapes={
            'tick':lambda d,w:d.line([(x-8,y),(x-2,y+7),(x+8,y-8)],fill=0,width=w),
            'slash':lambda d,w:d.line([(x-8,y+8),(x+8,y-8)],fill=0,width=w),
            'backslash':lambda d,w:d.line([(x-8,y-8),(x+8,y+8)],fill=0,width=w),
            'circle':lambda d,w:d.ellipse((x-8,y-8,x+8,y+8),outline=0,width=w),
            'plus':lambda d,w:(d.line([(x-9,y),(x+9,y)],fill=0,width=w),d.line([(x,y-9),(x,y+9)],fill=0,width=w)),
        }
        for name,shape in shapes.items():
            for w in (1,2,3):
                self.assertEqual(state(lambda d,im:shape(d,w)),'marked',dict(shape=name,width=w))
        # A loose scribble or a crossed-out X is one or the other; never
        # blank, and the preview says which.
        either={'scribble':lambda d,w:d.line([(x-9,y-6),(x+6,y+4),(x-8,y+7),(x+8,y-8),(x-8,y),(x+9,y+6)],fill=0,width=w),
                'X crossed out':lambda d,w:(d.line([(x-9,y-9),(x+9,y+9)],fill=0,width=w),d.line([(x-9,y+9),(x+9,y-9)],fill=0,width=w),d.line([(x-9,y),(x+9,y)],fill=0,width=w))}
        for name,shape in either.items():
            for w in (1,2,3):
                self.assertIn(state(lambda d,im:shape(d,w)),('marked','filled'),dict(shape=name,width=w))
        for w in (0,1):
            self.assertEqual(state(lambda d,im:d.ellipse((x-2-w,y-2-w,x+2+w,y+2+w),fill=0)),'unclear: only a dot')
        self.assertEqual(state(lambda d,im:d.rectangle((x-9,y-9,x+9,y+9),fill=0)),'filled')
        # Scribbled over densely: taken back as well.
        self.assertEqual(state(lambda d,im:[d.line([(x-10,y+k),(x+10,y+k+3)],fill=0,width=2) for k in range(-10,10,2)]),'filled')

    def test_bad_photos_change_nothing(self):
        row=self.doc['pages'][0]['rows'][0]
        # A dark shadow over the marked box.
        im=self.marked();d=ImageDraw.Draw(im)
        f=row['fields'][0];d.rectangle((f['x']*6-30,f['y']*6-30,f['x']*6+30,f['y']*6+30),fill=110)
        result=paper.scan(encoded(im),self.doc)
        self.assertEqual((result.get('marks'),[u['row'] for u in result.get('unclear',[])]),([],['0-0']),result)
        # A buckled page: a band of rows shifted sideways by 5mm, further
        # than a box is looked for around its place: those rows are unclear,
        # the rest of the page still reads.
        im=self.marked();band=im.crop((0,300,1260,420));im.paste(band,(30,300))
        result=paper.scan(encoded(im),self.doc)
        self.assertEqual(result.get('marks'),[dict(row='0-0',skipped=False,day='2026-09-28')],result)
        rows=self.doc['pages'][0]['rows']
        # (Rows whose boxes the band's edge cuts through as well.)
        self.assertEqual({u['row'] for u in result['unclear']},
                         {r['id'] for r in rows if 300/6<r['y']+2.4 and r['y']-2.4<420/6},result)
        # Most of the page unclear (a crumpled sheet, creases everywhere):
        # retake it, nothing read.
        im=self.marked();d=ImageDraw.Draw(im);rng=np.random.default_rng(7)
        for _ in range(120):
            x0,y0=rng.uniform(80,1180),rng.uniform(150,1600);a=rng.uniform(0,np.pi)
            d.line([(x0-200*np.cos(a),y0-200*np.sin(a)),(x0+200*np.cos(a),y0+200*np.sin(a))],fill=int(rng.uniform(90,160)),width=2)
        self.assertIn('Too much of the page is unclear',paper.scan(encoded(im),self.doc).get('error',''))
        # A thick X that runs over the box frame still reads as an X.
        im=self.blank.copy();pen_x(im,row['fields'][0],width=3,r=14)
        result=paper.scan(encoded(im),self.doc)
        self.assertEqual(result.get('marks'),[dict(row='0-0',skipped=False,day='2026-09-28')],result)

    def test_a_curled_page_in_shadow_sent_compressed(self):
        """Like a real photo that first went unread: the page hangs wavy and
        curled at the top (boxes up to 2mm off), a shadow lies over the QR
        corner, and the chat app shrank it to 1200x1600 and compressed it."""
        row,later=self.doc['pages'][0]['rows'][2],self.doc['pages'][0]['rows'][15]
        im=self.blank.copy()
        pen_x(im,row['fields'][1]);pen_x(im,later['fields'][0])
        a=np.asarray(im,dtype=float);h,w=a.shape
        yy,xx=np.mgrid[0:h,0:w].astype(float)
        # Waves: rows bend up to 2mm (12px), more towards the top.
        dy=12*np.sin(xx/w*np.pi*1.5)*(1-yy/h)+6*np.sin(yy/h*np.pi*3)
        dx=4*np.sin(yy/h*np.pi*2)
        src_y=np.clip(np.rint(yy+dy),0,h-1).astype(int);src_x=np.clip(np.rint(xx+dx),0,w-1).astype(int)
        a=a[src_y,src_x]
        # Shadow: the bottom-right third at 45%, with a soft edge.
        shade=np.clip((xx/w+yy/h-1.15)/.15,0,1);a=a*(1-.55*shade)
        page=Image.fromarray(a.astype(np.uint8))
        # The page in the middle of a dark wall, then shrunk to 1200x1600.
        photo=Image.new('L',(1700,2260),35);photo.paste(page,(220,240))
        photo=photo.resize((1200,1600),Image.Resampling.LANCZOS)
        data=encoded(photo,jpeg=True)
        result=paper.scan(data,self.doc)
        self.assertEqual(result.get('marks'),[dict(row=row['id'],skipped=False,day=row['fields'][1]['kind']),
                                             dict(row=later['id'],skipped=False,day=later['fields'][0]['kind'])],result)
        # The same photo with a dark shadow right over the marked box: that
        # duty is said to be unclear, not misread; the rest still reads.
        f=row['fields'][1];cx,cy=(f['x']*6+220)*1200/1700,(f['y']*6+240)*1600/2260
        d=ImageDraw.Draw(photo);d.rectangle((cx-60,cy-50,cx+60,cy+50),fill=60)
        result=paper.scan(encoded(photo,jpeg=True),self.doc)
        self.assertEqual(result.get('marks'),[dict(row=later['id'],skipped=False,day=later['fields'][0]['kind'])],result)
        self.assertIn(row['id'],[u['row'] for u in result['unclear']],result)

    def test_a_sheet_that_cannot_be_read_is_said_so(self):
        """Corner targets but no readable QR code: the bot answers instead of
        ignoring it like any other picture. Unrelated pictures stay ignored."""
        im=self.blank.copy();ImageDraw.Draw(im).rectangle((1000,1560,1260,1782),fill=255)
        self.assertEqual(paper.scan(encoded(im)),{'unreadable':True})
        self.assertEqual(paper.scan(encoded(Image.new('L',(800,600),128))),{})

    def test_legacy_v1_sheet_still_scans(self):
        """A v1 plan printed before (four corner QR codes, filled circles)."""
        rows=[]
        for i,days in enumerate([['2026-09-28','2026-09-29'],['2026-10-01','2026-10-02']]):
            y=64+i*25
            fields=[dict(id='done',x=114,y=y,label='Done'),dict(id='skip',x=148,y=y,label='Skipped')]
            fields+=[dict(id=d,x=114+j*11,y=y+9,label=d) for j,d in enumerate(days)]
            rows.append(dict(id=f'row-{i}',week=40,year=2026,name='Alice',label='Kitchen',status='',fields=fields))
        old=dict(id='0123456789abcdef0123456789abcdef',revision='abcdef012345',
                 pages=[dict(number=0,title='Kitchen',rows=rows)])
        with tempfile.TemporaryDirectory() as tmp:
            cwd=os.getcwd();os.chdir(tmp)
            try:
                paper.render(old,engine=ENGINE)
                subprocess.run(['pdftoppm','-scale-to-x','1050','-scale-to-y','1485','-png','-singlefile','sheet.pdf','v1'],check=True,stdout=subprocess.DEVNULL)
                im=Image.open('v1.png').convert('L')
            finally:
                os.chdir(cwd)
        self.assertEqual(paper.scan(encoded(im),old).get('marks'),[])
        d=ImageDraw.Draw(im)
        for f in rows[0]['fields']:
            if f['id'] in ('done','2026-09-29'):
                d.ellipse((f['x']*5-6,f['y']*5-6,f['x']*5+6,f['y']*5+6),fill=0)
        self.assertEqual(paper.scan(encoded(im),old).get('marks'),[dict(row='row-0',skipped=False,day='2026-09-29')])

    def test_the_worker_is_bounded(self):
        """The bot runs `python3 paper.py <mode>`: that process gets the limits."""
        script=Path(__file__).resolve().parent/'paper.py'
        probe=("import resource,runpy,sys;sys.argv=['paper.py','identify'];"
               "import paper_v1;paper_v1.limit_worker();"
               "print(resource.getrlimit(resource.RLIMIT_CPU)[0],resource.getrlimit(resource.RLIMIT_AS)[0])")
        out=subprocess.run([sys.executable,'-c',probe],cwd=script.parent,capture_output=True,text=True,check=True).stdout.split()
        self.assertEqual(out,['45',str(1536*1024**2)])
        self.assertIn('legacy.limit_worker()',script.read_text().split("__main__")[1])

    def test_printer_friendly_only_solid_black_and_white(self):
        """No colour and no grey anywhere: a weak printer renders those as a
        pale dot screen (and fine grey lines not at all)."""
        self.assertLessEqual(colours(Path('sheet.pdf').read_bytes()),
                             {(op,(v,)) for op in ('g','G') for v in (0.,1.)})

    def test_tex_source_is_ascii_and_the_qr_is_drawn(self):
        """Production compiles with Tectonic (XeTeX), tests with pdflatex. Raw
        non-ASCII characters print differently in the two (a raw "·" became
        "ů" and "–" vanished in Tectonic), and an embedded QR PNG came out as
        bare outlines there. So: ASCII source, QR as vector squares."""
        with tempfile.TemporaryDirectory() as tmp:
            cwd=os.getcwd();os.chdir(tmp)
            try:
                paper.render(self.doc,engine=ENGINE)
                tex=Path('sheet.tex').read_text()
            finally:
                os.chdir(cwd)
        bad=sorted({c for c in tex if ord(c)>127})
        self.assertEqual(bad,[],'non-ASCII characters in the TeX source')
        self.assertNotIn('includegraphics',tex)
        self.assertNotIn('imported',tex)
        self.assertIn(r'Zo{\"e}',tex)
        self.assertIn(r'\textperiodcentered{}',tex)
        self.assertIn('28 Dec -- 3 Jan',tex)
        self.assertEqual(paper.tex_escape('Kim 🌸 Straße'),r'Kim Stra{\ss}e')

    def test_revision_mismatch(self):
        self.assertIn('error',paper.scan(encoded(self.blank),dict(self.doc,revision='000000000000')))

    def test_two_pages(self):
        im=Image.new('L',(2520,1782),255);im.paste(self.blanks[0],(0,0));im.paste(self.blanks[1],(1260,0))
        self.assertIn('error',paper.scan(encoded(im),self.doc))

    def test_legacy_identity_is_still_recognized(self):
        data=(Path(__file__).resolve().parent/'fixtures/legacy-v1.png').read_bytes()
        self.assertEqual(paper.scan(data)['document'],'0123456789abcdef0123456789abcdef')

    @unittest.skipUnless(os.environ.get('PAPER_LAYOUT_FIXTURE'),'set PAPER_LAYOUT_FIXTURE to test Rust manifest')
    def test_actual_rust_layout_round_trip(self):
        doc=json.loads(Path(os.environ['PAPER_LAYOUT_FIXTURE']).read_text())
        for page in doc['pages']:
            # Without a QR read, the scanner falls back to these positions.
            self.assertEqual(page['fiducials'],paper.V2_FIDUCIALS)
            left,top,right,bottom=paper.V2_QR_AREA
            x,y=page['qr_center'];h=paper.QR_SIZE/2
            self.assertTrue(left<=x-h and x+h<=right and top<=y-h and y+h<=bottom,page['qr_center'])
        paper.render(doc,engine=ENGINE);im=raster(0,'actual')
        self.assertEqual(paper.scan(encoded(im),doc).get('marks'),[])
        row=doc['pages'][0]['rows'][0]
        pen_x(im,next(f for f in row['fields'] if f['kind']==row['start']))
        result=paper.scan(encoded(im),doc)
        self.assertEqual(result.get('marks'),[dict(row=row['id'],skipped=False,day=row['start'])],result)
        out=Path(__file__).resolve().parent.parent/'artifacts'
        (out/'paper-full-layout.pdf').write_bytes(Path('sheet.pdf').read_bytes())
        Image.open('actual.png').save(out/'paper-full-layout.png')

class TickSheetTests(unittest.TestCase):
    """The second model: one box per duty, done or not (the bot dates it)."""
    @classmethod
    def setUpClass(cls):
        cls.doc=tick_document();cls.tmp=tempfile.TemporaryDirectory();cls.old=os.getcwd();os.chdir(cls.tmp.name)
        paper.render(cls.doc,engine=ENGINE);cls.blank=raster(0,'tick')
        out=Path(__file__).resolve().parent.parent/'artifacts';out.mkdir(exist_ok=True)
        (out/'paper-tick.pdf').write_bytes(Path('sheet.pdf').read_bytes())
        subprocess.run(['pdftoppm','-r','150','-png','-singlefile','sheet.pdf','colour'],check=True,stdout=subprocess.DEVNULL)
        Image.open('colour.png').save(out/'paper-tick.png')

    @classmethod
    def tearDownClass(cls):os.chdir(cls.old);cls.tmp.cleanup()

    def field(self,rowid):
        return next(r for r in self.doc['pages'][0]['rows'] if r['id']==rowid)['fields'][0]

    def test_empty_sheet_has_no_marks(self):
        self.assertEqual(paper.scan(encoded(self.blank),self.doc).get('marks'),[])

    def test_ticks_side_by_side_are_read_without_a_day(self):
        im=self.blank.copy()
        for rowid in ['t-0-2','t-0-3','t-7-0','t-20-3']:pen_x(im,self.field(rowid))
        result=paper.scan(encoded(im),self.doc)
        self.assertEqual(result.get('marks'),[dict(row=r,skipped=False,day=None) for r in ['t-0-2','t-0-3','t-7-0','t-20-3']],result)

    def test_photographed_tick_sheet(self):
        im=self.blank.copy();pen_x(im,self.field('t-3-1'),width=3,uneven=True)
        src=[(0,0),(1260,0),(1260,1782),(0,1782)];dst=[(180,110),(1450,50),(1330,2080),(60,1860)]
        photo=im.transform((1560,2160),Image.Transform.PERSPECTIVE,paper.homography(dst,src),Image.Resampling.BICUBIC,fillcolor=220)
        result=paper.scan(encoded(photo,jpeg=True),self.doc)
        self.assertEqual(result.get('marks'),[dict(row='t-3-1',skipped=False,day=None)],result)
        # Known limit of sheets printed with 4.8mm boxes: a 0.5mm pen's X,
        # blurred by a tilted, compressed photo, can fill the small box. It
        # is then named as filled in (taken back), never dropped silently.
        result=paper.scan(encoded(photo.rotate(13,expand=True,fillcolor=220),jpeg=True),self.doc)
        done=[dict(row='t-3-1',skipped=False,day=None)]
        self.assertTrue(result.get('marks')==done or result.get('taken_back')==[dict(row='t-3-1',day=None)],result)

    def test_a_tick_counts_too(self):
        f=self.field('t-2-2');x,y=f['x']*6,f['y']*6;im=self.blank.copy()
        ImageDraw.Draw(im).line([(x-8,y),(x-2,y+7),(x+8,y-8)],fill=0,width=2)
        self.assertEqual(paper.scan(encoded(im),self.doc).get('marks'),[dict(row='t-2-2',skipped=False,day=None)])

    def test_ascii_source_and_no_raster_images(self):
        self.assertTrue(Path('sheet.tex').read_text().isascii())
        self.assertLessEqual(colours(Path('sheet.pdf').read_bytes()),
                             {(op,(v,)) for op in ('g','G') for v in (0.,1.)})
        images=subprocess.run(['pdfimages','-list','sheet.pdf'],capture_output=True,text=True).stdout
        self.assertEqual(len(images.splitlines()),2,images)

    def test_one_document_with_a_page_in_each_style(self):
        # Each group prints in its own style: a days page, then a tick page.
        days=document()['pages'][0];tick=dict(tick_document()['pages'][0],number=1)
        doc=dict(tick_document(),pages=[days,tick])
        os.chdir(tempfile.mkdtemp());paper.render(doc,engine=ENGINE)
        first,second=raster(0,'mixed-0'),raster(1,'mixed-1')
        pen_x(first,days['rows'][0]['fields'][0]);pen_x(second,self.field('t-4-2'))
        self.assertEqual(paper.scan(encoded(first),doc).get('marks'),
                         [dict(row=days['rows'][0]['id'],skipped=False,day=days['rows'][0]['fields'][0]['kind'])])
        self.assertEqual(paper.scan(encoded(second),doc).get('marks'),[dict(row='t-4-2',skipped=False,day=None)])
        os.chdir(self.tmp.name)

    @unittest.skipUnless(os.environ.get('PAPER_TICK_FIXTURE'),'set PAPER_TICK_FIXTURE to test the Rust tick manifest')
    def test_actual_rust_tick_layout_round_trip(self):
        doc=json.loads(Path(os.environ['PAPER_TICK_FIXTURE']).read_text())
        self.assertEqual([p['style'] for p in doc['pages']],['tick'])
        paper.render(doc,engine=ENGINE);im=raster(0,'actual-tick')
        self.assertEqual(paper.scan(encoded(im),doc).get('marks'),[])
        rows=[r for r in doc['pages'][0]['rows'] if r['fields']]
        week=[r for r in rows if (r['year'],r['week'])==(rows[0]['year'],rows[0]['week'])]
        for row in week:pen_x(im,row['fields'][0])
        result=paper.scan(encoded(im),doc)
        self.assertEqual(result.get('marks'),[dict(row=r['id'],skipped=False,day=None) for r in week],result)
        # On its 6mm boxes, any pen up to a 0.67mm marker reads as done,
        # photographed tilted and compressed too.
        blank=raster(0,'actual-tick');row=rows[5]
        for width in (2,3,4):
            im=blank.copy();pen_x(im,row['fields'][0],width=width,uneven=True,r=12)
            src=[(0,0),(1260,0),(1260,1782),(0,1782)];dst=[(180,110),(1450,50),(1330,2080),(60,1860)]
            photo=im.transform((1560,2160),Image.Transform.PERSPECTIVE,paper.homography(dst,src),Image.Resampling.BICUBIC,fillcolor=220)
            for angle in (0,13):
                result=paper.scan(encoded(photo.rotate(angle,expand=True,fillcolor=220),jpeg=True),doc)
                self.assertEqual(result.get('marks'),[dict(row=row['id'],skipped=False,day=None)],(width,angle,result))
        out=Path(__file__).resolve().parent.parent/'artifacts'
        (out/'paper-tick-full-layout.pdf').write_bytes(Path('sheet.pdf').read_bytes())


class ViewTests(unittest.TestCase):
    """`!plan pdf view`: the plan to hang up, nothing to tick, and it says so."""
    def view(self,doc):
        for page in doc['pages']:
            for row in page['rows']:row['fields']=[]
        return dict(doc,view_only=True)

    def check(self,doc,name):
        tmp=tempfile.mkdtemp();old=os.getcwd();os.chdir(tmp)
        try:
            paper.render(doc,engine=ENGINE)
            words=subprocess.run(['pdftotext','-layout','sheet.pdf','-'],capture_output=True,text=True,check=True).stdout
            self.assertEqual(words.count('VIEW ONLY, NOT FOR TICKING'),len(doc['pages']),words[:400])
            self.assertEqual(words.count('this is not the sheet to tick'),len(doc['pages']))
            self.assertNotIn('Put one clear X',words)
            for i in range(len(doc['pages'])):
                im=raster(i,f'view-{i}')
                # Nothing a photo could be taken for: no QR, no corner targets.
                self.assertEqual(paper.decode(im),[])
                with self.assertRaises(ValueError):paper.fiducials(im)
                self.assertEqual(paper.scan(encoded(im)),{})
                self.assertEqual(paper.scan(encoded(im),doc),{})
            self.assertTrue(Path('sheet.tex').read_text().isascii())
            self.assertLessEqual(colours(Path('sheet.pdf').read_bytes()),
                                 {(op,(v,)) for op in ('g','G') for v in (0.,1.)})
            out=Path(__file__).resolve().parent.parent/'artifacts';out.mkdir(exist_ok=True)
            (out/f'paper-view-{name}.pdf').write_bytes(Path('sheet.pdf').read_bytes())
        finally:os.chdir(old)

    def test_days_and_tick_pages_as_views(self):
        doc=self.view(document());doc['pages'].append(dict(self.view(tick_document())['pages'][0],number=3))
        self.check(doc,'example')

    @unittest.skipUnless(os.environ.get('PAPER_VIEW_FIXTURE'),'set PAPER_VIEW_FIXTURE to test the Rust view manifest')
    def test_actual_rust_view(self):
        doc=json.loads(Path(os.environ['PAPER_VIEW_FIXTURE']).read_text())
        self.assertTrue(doc['view_only'])
        self.check(doc,'full-layout')


if __name__=='__main__':unittest.main()
