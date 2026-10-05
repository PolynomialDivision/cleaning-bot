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
    for number,(title,mode,count) in enumerate([('Kitchen','twice',16),('Upper Floor','slots',16),('Bathroom','weekly',16)]):
        rows=[]
        for i in range(count):
            week_offset=i//2 if mode!='weekly' else i
            start=dt.date(2026,9,28)+dt.timedelta(weeks=week_offset)
            if mode=='twice': start+=dt.timedelta(days=3*(i%2))
            end=start+dt.timedelta(days=1 if mode=='twice' else 6)
            y=58.25+i*12.5;rowid=f'{number}-{i}'
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
        pages.append(dict(number=number,title=title,rooms={'Kitchen':'Counters · Sink · Floor','Upper Floor':'Stairs · Hallway','Bathroom':'Shower · Toilet · Sink'}[title],
                          rows=rows,fiducials=[[10,10],[200,10],[200,287],[10,287]],qr_center=[187,277]))
    return dict(layout_version=2,id='0123456789abcdef0123456789abcdef',revision='abcdef012345',pages=pages)


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

    def test_multiple_marks_rejected(self):
        for fields in [('2026-09-28','2026-09-29'),('2026-09-28','2026-09-29','2026-10-01')]:
            self.assertIn('error',paper.scan(encoded(self.marked(fields=fields)),self.doc))

    def test_tick_slash_dot_fill_and_scribble_rejected(self):
        f=self.doc['pages'][0]['rows'][0]['fields'][0];x,y=f['x']*6,f['y']*6
        for shape in ['tick','slash','dot','fill','scribble']:
            im=self.blank.copy();d=ImageDraw.Draw(im)
            if shape=='tick':d.line([(x-8,y),(x-2,y+7),(x+8,y-8)],fill=0,width=2)
            elif shape=='slash':d.line([(x-8,y+8),(x+8,y-8)],fill=0,width=2)
            elif shape=='dot':d.ellipse((x-2,y-2,x+2,y+2),fill=0)
            elif shape=='fill':d.rectangle((x-9,y-9,x+9,y+9),fill=0)
            else:d.line([(x-9,y-6),(x+6,y+4),(x-8,y+7),(x+8,y-8),(x-8,y),(x+9,y+6)],fill=0,width=2)
            result=paper.scan(encoded(im),self.doc)
            self.assertIn('error',result,(shape,result))

    def test_missing_marker_partial_blur_unrelated(self):
        self.assertEqual(paper.scan(encoded(Image.new('L',(1260,1782),200)),self.doc),{})
        im=self.blank.copy();ImageDraw.Draw(im).rectangle((40,40,85,85),fill=255)
        self.assertIn('error',paper.scan(encoded(im),self.doc))
        self.assertIn('error',paper.scan(encoded(self.blank.crop((0,150,1260,1782))),self.doc))
        self.assertFalse(paper.scan(encoded(self.marked().filter(ImageFilter.GaussianBlur(5))),self.doc).get('marks'))

    def test_classifier_reads_every_x_and_no_other_shape(self):
        """Directly on the 6px/mm raster (the scanner's own coordinate
        system), so many variants stay fast: pen 0.17–0.67mm, grey to black,
        off-centre, small to large, uneven strokes."""
        field=self.doc['pages'][0]['rows'][0]['fields'][0]
        x,y=field['x']*6,field['y']*6
        def value(draw):
            im=self.blank.copy();draw(ImageDraw.Draw(im),im)
            try: return paper.field_value(np.asarray(im,dtype=float),field)
            except ValueError as e: return str(e)
        for width in (1,2,3,4):
            for gray in (10,40,90):
                for offset in ((0,0),(2,-1),(-2,2),(1,2)):
                    for r in (7,9,11):
                        for uneven in (False,True):
                            case=dict(width=width,gray=gray,offset=offset,r=r,uneven=uneven)
                            got=value(lambda d,im:pen_x(im,field,width,offset,gray,uneven,r))
                            self.assertIs(got,True,case)
        shapes={
            'tick':lambda d,w:d.line([(x-8,y),(x-2,y+7),(x+8,y-8)],fill=0,width=w),
            'slash':lambda d,w:d.line([(x-8,y+8),(x+8,y-8)],fill=0,width=w),
            'backslash':lambda d,w:d.line([(x-8,y-8),(x+8,y+8)],fill=0,width=w),
            'dot':lambda d,w:d.ellipse((x-2-w,y-2-w,x+2+w,y+2+w),fill=0),
            'fill':lambda d,w:d.rectangle((x-9,y-9,x+9,y+9),fill=0),
            'circle':lambda d,w:d.ellipse((x-8,y-8,x+8,y+8),outline=0,width=w),
            'plus':lambda d,w:(d.line([(x-9,y),(x+9,y)],fill=0,width=w),d.line([(x,y-9),(x,y+9)],fill=0,width=w)),
            'three arms':lambda d,w:(d.line([(x-8,y-8),(x+8,y+8)],fill=0,width=w),d.line([(x,y),(x+8,y-8)],fill=0,width=w)),
            'grid':lambda d,w:[d.line(p,fill=0,width=w) for p in ([(x-9,y-3),(x+9,y-3)],[(x-9,y+3),(x+9,y+3)],[(x-3,y-9),(x-3,y+9)],[(x+3,y-9),(x+3,y+9)])],
            'X crossed out':lambda d,w:(d.line([(x-9,y-9),(x+9,y+9)],fill=0,width=w),d.line([(x-9,y+9),(x+9,y-9)],fill=0,width=w),d.line([(x-9,y),(x+9,y)],fill=0,width=w)),
            'scribble':lambda d,w:d.line([(x-9,y-6),(x+6,y+4),(x-8,y+7),(x+8,y-8),(x-8,y),(x+9,y+6)],fill=0,width=w),
        }
        for name,shape in shapes.items():
            # Known limit: a loose scribble with a 0.5mm+ marker can look
            # like an X in a 4.8mm box; the preview before applying shows it.
            widths=(1,2) if name=='scribble' else (1,2,3)
            for w in widths:
                got=value(lambda d,im:shape(d,w))
                self.assertIsNot(got,True,dict(shape=name,width=w))

    def test_bad_photos_change_nothing(self):
        row=self.doc['pages'][0]['rows'][0]
        # A dark shadow over the marked box.
        im=self.marked();d=ImageDraw.Draw(im)
        f=row['fields'][0];d.rectangle((f['x']*6-30,f['y']*6-30,f['x']*6+30,f['y']*6+30),fill=110)
        self.assertFalse(paper.scan(encoded(im),self.doc).get('marks'))
        # A buckled page: a band of rows shifted sideways by 1mm.
        im=self.marked();band=im.crop((0,300,1260,420));im.paste(band,(6,300))
        self.assertFalse(paper.scan(encoded(im),self.doc).get('marks'))
        # A thick X that runs over the box frame still reads as an X.
        im=self.blank.copy();pen_x(im,row['fields'][0],width=3,r=14)
        result=paper.scan(encoded(im),self.doc)
        self.assertEqual(result.get('marks'),[dict(row='0-0',skipped=False,day='2026-09-28')],result)

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
            self.assertEqual(page['qr_center'],paper.V2_QR)
        paper.render(doc,engine=ENGINE);im=raster(0,'actual')
        self.assertEqual(paper.scan(encoded(im),doc).get('marks'),[])
        row=doc['pages'][0]['rows'][0]
        pen_x(im,next(f for f in row['fields'] if f['kind']==row['start']))
        result=paper.scan(encoded(im),doc)
        self.assertEqual(result.get('marks'),[dict(row=row['id'],skipped=False,day=row['start'])],result)
        out=Path(__file__).resolve().parent.parent/'artifacts'
        (out/'paper-full-layout.pdf').write_bytes(Path('sheet.pdf').read_bytes())
        Image.open('actual.png').save(out/'paper-full-layout.png')

if __name__=='__main__':unittest.main()
