"""Paper protocol v1. Fixed millimetre coordinates; QR-only identification; no OCR."""
import ctypes as C
import ctypes.util
import io
import json
import os
import resource
import subprocess
import sys
from pathlib import Path
resource.setrlimit(resource.RLIMIT_AS, (1536 * 1024**2, 1536 * 1024**2))
resource.setrlimit(resource.RLIMIT_CPU, (45, 45))
os.environ['OPENBLAS_NUM_THREADS'] = '1'
import numpy as np
from PIL import Image, ImageOps
Image.MAX_IMAGE_PIXELS = 24_000_000
CORNERS = [(19,19),(191,19),(191,278),(19,278)]
SCALE = 5

def marker(doc,page,corner):
    return f"CB1:{doc['id']}:{doc['revision']}:{page}:{corner}"

def decode(im):
    z=C.CDLL(ctypes.util.find_library('zbar'))
    signatures={
        'image_scanner_create':(C.c_void_p,[]), 'image_scanner_destroy':(None,[C.c_void_p]),
        'image_scanner_set_config':(C.c_int,[C.c_void_p,C.c_int,C.c_int,C.c_int]),
        'image_create':(C.c_void_p,[]), 'image_destroy':(None,[C.c_void_p]),
        'image_set_format':(None,[C.c_void_p,C.c_ulong]),
        'image_set_size':(None,[C.c_void_p,C.c_uint,C.c_uint]),
        'image_set_data':(None,[C.c_void_p,C.c_void_p,C.c_ulong,C.c_void_p]),
        'scan_image':(C.c_int,[C.c_void_p,C.c_void_p]),
        'image_first_symbol':(C.c_void_p,[C.c_void_p]), 'symbol_next':(C.c_void_p,[C.c_void_p]),
        'symbol_get_data':(C.c_char_p,[C.c_void_p]), 'symbol_get_loc_size':(C.c_uint,[C.c_void_p]),
        'symbol_get_loc_x':(C.c_int,[C.c_void_p,C.c_uint]), 'symbol_get_loc_y':(C.c_int,[C.c_void_p,C.c_uint]),
    }
    for name,(res,args) in signatures.items():
        f=getattr(z,'zbar_'+name);f.restype=res;f.argtypes=args
    scanner=z.zbar_image_scanner_create();image=z.zbar_image_create()
    try:
        z.zbar_image_scanner_set_config(scanner,0,0,0)
        z.zbar_image_scanner_set_config(scanner,64,0,1)
        z.zbar_image_set_format(image,int.from_bytes(b'Y800','little'))
        z.zbar_image_set_size(image,*im.size)
        data=C.create_string_buffer(im.tobytes())
        z.zbar_image_set_data(image,data,len(data)-1,None)
        z.zbar_scan_image(scanner,image)
        found=[];symbol=z.zbar_image_first_symbol(image)
        while symbol:
            payload=z.zbar_symbol_get_data(symbol).decode('ascii',errors='replace')
            points=[(z.zbar_symbol_get_loc_x(symbol,i),z.zbar_symbol_get_loc_y(symbol,i)) for i in range(z.zbar_symbol_get_loc_size(symbol))]
            parts=payload.split(':')
            if len(parts)==5 and parts[0]=='CB1' and len(parts[1])==32 and len(parts[2])==12 and parts[3].isdigit() and parts[4] in ('0','1','2','3') and len(points)==4:
                p=np.array([[x,y,1.] for x,y in points])
                center=np.cross(np.cross(p[0],p[2]),np.cross(p[1],p[3]))
                if abs(center[2])>1e-6:found.append((parts,(center[:2]/center[2]).tolist()))
            symbol=z.zbar_symbol_next(symbol)
        return found
    finally:
        z.zbar_image_destroy(image);z.zbar_image_scanner_destroy(scanner)

def markers(im):
    found=decode(im)
    # ZBar occasionally misses a QR at one raster orientation. A lossless
    # half-turn offers another scan direction without weakening validation.
    if found and len(found)<4:
        known={tuple(parts) for parts,_ in found}
        for parts,(x,y) in decode(im.transpose(Image.Transpose.ROTATE_180)):
            if tuple(parts) not in known:
                found.append((parts,[im.width-1-x,im.height-1-y]));known.add(tuple(parts))
    return found

def homography(src,dst):
    a=[];b=[]
    for (x,y),(u,v) in zip(src,dst):
        a.extend([[x,y,1,0,0,0,-u*x,-u*y],[0,0,0,x,y,1,-v*x,-v*y]]);b.extend([u,v])
    return np.linalg.solve(a,b)

def normalize(im,found):
    corners={int(parts[4]):point for parts,point in found}
    if len(found)!=4 or len(corners)!=4:raise ValueError('Show all four corner codes on one page')
    dst=[corners[i] for i in range(4)]
    edges=[np.linalg.norm(np.array(dst[i])-dst[(i+1)%4]) for i in range(4)]
    if min(edges)<450 or max(edges)/min(edges)>3:raise ValueError('Photo too small or angle too steep')
    h=homography([(x*SCALE,y*SCALE) for x,y in CORNERS],dst)
    return im.transform((210*SCALE,297*SCALE),Image.Transform.PERSPECTIVE,h,Image.Resampling.BICUBIC,fillcolor=255)

def field_value(arr,field):
    x,y=field['x']*SCALE,field['y']*SCALE
    yy,xx=np.mgrid[int(y-3*SCALE):int(y+3*SCALE)+1,int(x-3*SCALE):int(x+3*SCALE)+1]
    rr=np.sqrt((xx-x)**2+(yy-y)**2)/SCALE
    patch=arr[int(y-3*SCALE):int(y+3*SCALE)+1,int(x-3*SCALE):int(x+3*SCALE)+1]
    bg=np.median(patch[(rr>2.1)&(rr<2.8)])
    if bg<110 or np.std(patch[(rr>2.1)&(rr<2.8)])>35:raise ValueError('Shadow or scribble near a field; retake the photo')
    border=patch[(rr>1.35)&(rr<1.85)]
    if np.mean(border<bg*0.7)<0.18:raise ValueError('Field outline unclear; flatten the sheet and retake')
    density=np.mean(patch[rr<1.0]<bg*0.65)
    if density<0.06:return False
    if density>0.35:return True
    raise ValueError('Ambiguous mark; fill the selected bubble solidly')

def read_marks(im,page):
    marks=[];arr=np.array(im,dtype=float)
    for row in page['rows']:
        selected=[f.get('kind',f['id']) for f in row['fields'] if field_value(arr,f)]
        if not selected:continue
        statuses=[s for s in selected if s in ('done','skip')]
        days=[s for s in selected if s not in ('done','skip')]
        if len(statuses)!=1 or (statuses[0]=='done' and len(days)!=1) or (statuses[0]=='skip' and days):raise ValueError('Select Done and one day, or Skipped alone')
        marks.append({'row':row['id'],'skipped':statuses[0]=='skip','day':days[0] if days else None})
    return marks

def scan(data,doc=None):
    try:
        im=ImageOps.exif_transpose(Image.open(io.BytesIO(data)))
        if im.width*im.height>24_000_000:return {}
        im=im.convert('L');im.thumbnail((2600,3600));found=markers(im)
    except (OSError,ValueError,Image.DecompressionBombError):return {}
    if not found:return {}
    parts=found[0][0]
    result={'document':parts[1],'revision':parts[2],'page':int(parts[3])}
    if doc is None:return result
    try:
        if any(p[1:4]!=parts[1:4] for p,_ in found):raise ValueError('Photograph only one page at a time')
        if parts[1]!=doc['id'] or parts[2]!=doc['revision']:raise ValueError('Unknown document revision')
        result['marks']=read_marks(normalize(im,found),doc['pages'][int(parts[3])])
    except (ValueError,IndexError,np.linalg.LinAlgError) as e:result['error']=str(e)
    return result

def tex_escape(s):
    return ''.join({'\\':r'\textbackslash{}','&':r'\&','%':r'\%','$':r'\$','#':r'\#','_':r'\_','{':r'\{','}':r'\}','~':r'\textasciitilde{}','^':r'\textasciicircum{}'}.get(c,c) for c in s)

def render(doc,engine='tectonic'):
    tex=[r'\documentclass[a4paper]{article}',r'\usepackage[margin=0mm]{geometry}',r'\usepackage{graphicx,tikz}',r'\usepackage[T1]{fontenc}',r'\usepackage[utf8]{inputenc}',r'\usepackage{lmodern}',r'\renewcommand{\familydefault}{\sfdefault}',r'\pagestyle{empty}',r'\begin{document}']
    for page in doc['pages']:
        if page['number']:tex.append(r'\newpage')
        tex.append(r'\null\begin{tikzpicture}[remember picture,overlay,x=1mm,y=-1mm,shift={(current page.north west)}]')
        for i,(x,y) in enumerate(CORNERS):
            name=f"qr-{page['number']}-{i}.png"
            subprocess.run(['qrencode','-l','M','-s','8','-m','4','-o',name,marker(doc,page['number'],i)],check=True,stdout=subprocess.DEVNULL)
            tex.append(fr'\node[inner sep=0] at ({x},{y}) {{\includegraphics[width=22mm]{{{name}}}}};')
        def text(x,y,s,size=10,width=175):
            tex.append(fr'\node[anchor=north west,inner sep=0,text width={width}mm,font=\fontsize{{{size}}}{{{size+2}}}\selectfont] at ({x},{y}) {{{tex_escape(s)}}};')
        text(36,10,'CLEANING PLAN',19,136);text(36,21,page['title'][:60],13,136)
        text(36,28,page.get('rooms','')[:160],8,136)
        text(14,36,'Fill circles solidly: Done + one day, or Skipped. Leave open duties blank.',9)
        text(14,42,'Send a photo of this full page to the bot. Review and confirm there.',9)
        for i,row in enumerate(page['rows']):
            y=56+i*25
            tex.append(fr'\draw[gray!50] (14,{y-2}) -- (196,{y-2});')
            text(14,y,f"Week {row['week']} / {row['year']}  ·  {row['name'][:30]}",11,94)
            text(14,y+7,row['label'],9,94);text(14,y+18,'Notes: ______________________',8,90)
            if row['status']:text(114,y+7,row['status'],10,80)
            for f in row['fields']:
                tex.append(fr"\draw[line width=0.35mm] ({f['x']},{f['y']}) circle (1.6mm);")
                if f.get('kind',f['id']) in ('done','skip'):text(f['x']+4,f['y']-2,f['label'],9,28)
                else:text(f['x']-3,f['y']+3.2,f['label'],7,11)
        text(36,273,f"Page {page['number']+1}/{len(doc['pages'])} · Keep all four corner codes visible",8,137)
        text(36,280,'Paper is a proposal. The bot checks the current plan before applying it.',8,137)
        tex.append(r'\end{tikzpicture}')
    tex.append(r'\end{document}');Path('sheet.tex').write_text('\n'.join(tex))
    args=['tectonic','sheet.tex'] if engine=='tectonic' else ['pdflatex','-interaction=nonstopmode','-halt-on-error','sheet.tex']
    # Absolute overlay coordinates require the second LaTeX pass.
    for _ in range(2 if engine!='tectonic' else 1):
        proc=subprocess.run(args,stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=45)
        if proc.returncode:raise RuntimeError(proc.stdout.decode(errors='replace')[-2000:]+proc.stderr.decode(errors='replace')[-2000:])
    return Path('sheet.pdf').read_bytes()

if __name__=='__main__':
    mode=sys.argv[1];data=Path('input').read_bytes()
    if mode=='pdf':sys.stdout.buffer.write(render(json.loads(data)))
    else:print(json.dumps(scan(data,json.loads(Path('manifest.json').read_text()) if mode=='scan' else None)))
