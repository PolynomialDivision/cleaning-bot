"""End-to-end fixtures: render actual PDF, rasterize, mark, photograph, scan."""
import datetime
import io
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from PIL import Image, ImageDraw, ImageFilter
import paper


def document():
    rows=[]
    for i,days in enumerate([['2026-09-28','2026-09-29'],['2026-10-01','2026-10-02'],['2026-09-28','2026-09-29','2026-09-30','2026-10-01','2026-10-02','2026-10-03','2026-10-04']]):
        y=64+i*25
        fields=[dict(id='done',x=114,y=y,label='Done'),dict(id='skip',x=148,y=y,label='Skipped')]
        fields.extend(dict(id=d,x=114+j*11,y=y+9,label=datetime.date.fromisoformat(d).strftime('%a')) for j,d in enumerate(days))
        rows.append(dict(id=f'row-{i}',week=40,year=2026,name=['Alice','Bob','Carol'][i],label=['Mon–Tue · Kitchen','Thu–Fri · Kitchen','3+4 Floor · Stairs'][i],status='',fields=fields))
    return dict(id='0123456789abcdef0123456789abcdef',revision='abcdef012345',pages=[dict(number=0,title='2nd Floor / 3+4 Floor',rows=rows)])


def encoded(im):
    b=io.BytesIO();im.save(b,format='PNG');return b.getvalue()


class PaperTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.doc=document();cls.tmp=tempfile.TemporaryDirectory();cls.old=os.getcwd();os.chdir(cls.tmp.name)
        paper.render(cls.doc,engine='pdflatex')
        subprocess.run(['pdftoppm','-scale-to-x','1050','-scale-to-y','1485','-png','-singlefile','sheet.pdf','sheet'],check=True,stdout=subprocess.DEVNULL)
        cls.blank=Image.open('sheet.png').convert('L')
        out=Path(__file__).resolve().parent.parent/'artifacts'
        out.mkdir(exist_ok=True)
        (out/'paper-example.pdf').write_bytes(Path('sheet.pdf').read_bytes())
        cls.blank.save(out/'paper-example.png')

    @classmethod
    def tearDownClass(cls):os.chdir(cls.old);cls.tmp.cleanup()

    def marked(self,row=0,fields=('done','2026-09-28')):
        im=self.blank.copy();draw=ImageDraw.Draw(im)
        for f in self.doc['pages'][0]['rows'][row]['fields']:
            if f['id'] in fields:
                x,y=f['x']*5,f['y']*5;draw.ellipse((x-6,y-6,x+6,y+6),fill=0)
        return im

    def test_pdf_identity_and_empty_fields(self):
        self.assertEqual(len(paper.decode(self.blank)),4)
        result=paper.scan(encoded(self.blank),self.doc)
        self.assertNotIn('error',result);self.assertEqual(result['marks'],[])

    def test_done_and_permitted_day(self):
        result=paper.scan(encoded(self.marked()),self.doc)
        self.assertNotIn('error',result);self.assertEqual(result['marks'],[dict(row='row-0',skipped=False,day='2026-09-28')])

    def test_skipped(self):
        result=paper.scan(encoded(self.marked(1,('skip',))),self.doc)
        self.assertEqual(result.get('marks'),[dict(row='row-1',skipped=True,day=None)],result)

    def test_multi_slot_weekly_day(self):
        result=paper.scan(encoded(self.marked(2,('done','2026-10-04'))),self.doc)
        self.assertEqual(result.get('marks'),[dict(row='row-2',skipped=False,day='2026-10-04')],result)

    def test_rotation_and_perspective(self):
        im=self.marked();src=[(0,0),(1050,0),(1050,1485),(0,1485)]
        dst=[(120,90),(1200,30),(1120,1700),(30,1550)]
        h=paper.homography(dst,src)
        photo=im.transform((1300,1800),Image.Transform.PERSPECTIVE,h,Image.Resampling.BICUBIC,fillcolor=210)
        for angle in (0,90,180,270):
            result=paper.scan(encoded(photo.rotate(angle,expand=True)),self.doc)
            self.assertEqual(result.get('marks'),[dict(row='row-0',skipped=False,day='2026-09-28')],result)

    def test_ambiguous_status_or_day(self):
        for fields in [('done','skip'),('done',),('2026-09-28',),('done','2026-09-28','2026-09-29')]:
            self.assertIn('error',paper.scan(encoded(self.marked(fields=fields)),self.doc))

    def test_partial_blurry_and_unrelated(self):
        self.assertEqual(paper.scan(encoded(Image.new('L',(1050,1485),200)),self.doc),{})
        result=paper.scan(encoded(self.blank.crop((0,0,1050,1000))),self.doc)
        self.assertIn('error',result)
        result=paper.scan(encoded(self.marked().filter(ImageFilter.GaussianBlur(8))),self.doc)
        self.assertFalse(result.get('marks'))

    def test_revision_mismatch(self):
        doc=dict(self.doc,revision='000000000000')
        self.assertIn('error',paper.scan(encoded(self.blank),doc))

    def test_two_pages(self):
        image=Image.new('L',(2100,1485),255);image.paste(self.blank,(0,0));image.paste(self.blank,(1050,0))
        self.assertFalse(paper.scan(encoded(image),self.doc).get('marks'))

    def test_low_confidence_and_shadow(self):
        im=self.blank.copy();d=ImageDraw.Draw(im)
        # A small dot is neither confidently blank nor a filled circle.
        d.ellipse((568,318,572,322),fill=0)
        self.assertIn('error',paper.scan(encoded(im),self.doc))
        im=self.marked();d=ImageDraw.Draw(im);d.rectangle((550,300,590,345),fill=80)
        self.assertIn('error',paper.scan(encoded(im),self.doc))

    @unittest.skipUnless(os.environ.get('PAPER_LAYOUT_FIXTURE'),'set PAPER_LAYOUT_FIXTURE to test Rust manifest')
    def test_actual_rust_layout_round_trip(self):
        doc=json.loads(Path(os.environ['PAPER_LAYOUT_FIXTURE']).read_text())
        paper.render(doc,engine='pdflatex')
        subprocess.run(['pdftoppm','-f','1','-scale-to-x','1050','-scale-to-y','1485','-png','-singlefile','sheet.pdf','actual'],check=True,stdout=subprocess.DEVNULL)
        im=Image.open('actual.png').convert('L')
        result=paper.scan(encoded(im),doc)
        self.assertEqual(result.get('marks'),[],result)
        row=doc['pages'][0]['rows'][0]
        for f in row['fields']:
            if f['kind'] in ('done',row['start']):
                x,y=f['x']*5,f['y']*5
                ImageDraw.Draw(im).ellipse((x-6,y-6,x+6,y+6),fill=0)
        result=paper.scan(encoded(im),doc)
        self.assertEqual(result.get('marks'),[dict(row=row['id'],skipped=False,day=row['start'])],result)
        out=Path(__file__).resolve().parent.parent/'artifacts'
        (out/'paper-full-layout.pdf').write_bytes(Path('sheet.pdf').read_bytes())
        Image.open('actual.png').save(out/'paper-full-layout.png')

if __name__=='__main__':unittest.main()
