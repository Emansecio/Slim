"""Visual scenario: animated 3D SVG page, checked in a headless browser and captured per arm."""
import html
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

CHROME_CANDIDATES = (
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    '/Applications/Chromium.app/Contents/MacOS/Chromium',
    '/Applications/Brave Browser.app/Contents/MacOS/Brave Browser',
    r'C:\Program Files\Google\Chrome\Application\chrome.exe',
    r'C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe',
)
CHROME_NAMES = ('google-chrome', 'google-chrome-stable', 'chromium', 'chromium-browser', 'chrome')
# Frames captured for the side-by-side gallery; t and t+8 must match (8 s period).
CAPTURE_TIMES = (0, 1, 2.5, 4)

PROMPT = ('Read SPEC.md and build the page. Run python check.py and fix failures. '
          'Do not change SPEC.md or check.py. Do not install dependencies or use the network. '
          'Finish with a brief summary.')

SPEC = '''Create index.html: one self-contained page (inline CSS and JavaScript only; no external
URLs, imports, fonts, images, canvas or WebGL) that renders an animated 3D scene in SVG.

Scene: a solid regular icosahedron (12 vertices, 20 triangular faces) with perspective
projection. It rotates about the vertical axis, one full turn every 8 seconds, while the
tilt about the horizontal axis oscillates as 20 * sin(2 * pi * t / 8) degrees. Draw only
faces that face the viewer (back-face culling), sorted back to front. Use flat Lambert
shading from one fixed directional light: each face fill comes from its normal, so the
visible faces show several distinct shades of one hue. Add a soft elliptical ground shadow
under the object, a gradient background, the heading "Slim Bench" and a caption with the
current vertical-axis angle in degrees. Everything fits 800x600 without scrolling.

DOM contract (checked in a headless browser):
- <svg id="scene" viewBox="0 0 800 600"> holds the faces as <polygon class="face">
  elements with three points each, in drawing order (back to front).
- Time t in seconds comes from the query string ?t=<seconds>. When present, render exactly
  that frame synchronously while the page loads and do not animate. The scene is a pure
  function of t, so ?t=0 and ?t=8 render identically. Without ?t, animate with
  requestAnimationFrame.
- After each render set document.body.dataset.faces to the number of drawn polygons and
  document.body.dataset.angle to the vertical-axis angle in whole degrees (0-359).
- With prefers-reduced-motion: reduce and no ?t, render t=0 and do not animate.
'''

CHECK = r'''import html.parser
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

page = Path('index.html')
assert page.is_file(), 'index.html missing'
source = page.read_text(encoding='utf-8')
assert len(source.encode('utf-8')) <= 64 * 1024, 'index.html exceeds 64 KiB'
lowered = source.lower()
for banned in ('<canvas', 'getcontext(', 'webgl', '<link', '@import', '<img', 'import(', 'fetch(', 'xmlhttprequest'):
    assert banned not in lowered, 'forbidden construct: ' + banned
assert not re.search(r'(src|href)\s*=\s*["\'](?!#)', lowered), 'external src/href is not allowed'
assert not re.search(r'url\(\s*["\']?(?!#)', lowered), 'only url(#id) references are allowed'
assert not re.search(r'https?://(?!www\.w3\.org/)', lowered), 'network URL found'
assert 'requestanimationframe' in lowered, 'animation must use requestAnimationFrame'
assert 'prefers-reduced-motion' in lowered, 'prefers-reduced-motion is not handled'

def chrome():
    for name in ('CHROME', 'CHROME_PATH'):
        if os.environ.get(name) and Path(os.environ[name]).exists():
            return os.environ[name]
    for path in ('/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
                 '/Applications/Chromium.app/Contents/MacOS/Chromium',
                 '/Applications/Brave Browser.app/Contents/MacOS/Brave Browser',
                 r'C:\Program Files\Google\Chrome\Application\chrome.exe',
                 r'C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe'):
        if Path(path).exists():
            return path
    for name in ('google-chrome', 'google-chrome-stable', 'chromium', 'chromium-browser', 'chrome'):
        if shutil.which(name):
            return shutil.which(name)
    raise SystemExit('FAIL: a Chrome/Chromium browser is required for the DOM checks')

class Scene(html.parser.HTMLParser):
    def __init__(self):
        super().__init__()
        self.body, self.svg, self.faces = {}, None, []
    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag == 'body':
            self.body = attrs
        elif tag == 'svg' and attrs.get('id') == 'scene':
            self.svg = attrs
        elif tag == 'polygon' and 'face' in (attrs.get('class') or '').split():
            self.faces.append(attrs)

def render(t):
    with tempfile.TemporaryDirectory(prefix='bench-chrome-') as profile:
        result = subprocess.run([chrome(), '--headless=new', '--disable-gpu', '--no-first-run',
                                 '--no-default-browser-check', '--disable-extensions',
                                 '--user-data-dir=' + profile, '--virtual-time-budget=1000',
                                 '--window-size=800,600', '--dump-dom',
                                 page.resolve().as_uri() + '?t=' + str(t)],
                                capture_output=True, text=True, timeout=60)
    assert result.returncode == 0, 'browser failed: ' + result.stderr[-500:]
    scene = Scene()
    scene.feed(result.stdout)
    return scene

def points(face):
    values = [float(v) for v in re.split(r'[\s,]+', (face.get('points') or '').strip()) if v]
    assert len(values) == 6, 'each face needs three points: ' + str(face.get('points'))
    return [(values[i], values[i + 1]) for i in (0, 2, 4)]

frames = {}
for t in (0, 1, 2.5, 8):
    scene = render(t)
    assert scene.svg is not None, 'svg#scene not found in the rendered DOM'
    assert re.sub(r'\s+', ' ', scene.svg.get('viewbox', '')).strip() == '0 0 800 600', 'viewBox must be 0 0 800 600'
    count = len(scene.faces)
    assert 6 <= count <= 14, 'expected only the visible faces of an icosahedron, got %d' % count
    assert scene.body.get('data-faces') == str(count), 'data-faces must equal the drawn polygons'
    expected = round(t * 45) % 360
    angle = int(scene.body.get('data-angle', '-999'))
    assert min(abs(angle - expected), 360 - abs(angle - expected)) <= 1, 'data-angle %s, expected %s' % (angle, expected)
    shape = [points(face) for face in scene.faces]
    for triangle in shape:
        assert all(0 <= x <= 800 and 0 <= y <= 600 for x, y in triangle), 'face outside the viewBox'
    fills = {(face.get('fill') or face.get('style') or '').strip().lower() for face in scene.faces}
    assert len(fills) >= 4, 'shading must produce at least 4 distinct face fills, got %d' % len(fills)
    frames[t] = sorted(sorted(triangle) for triangle in shape)

def same(a, b):
    # Ties in the painter's sort may reorder equal-depth faces; compare as sets within 0.5 px.
    return len(a) == len(b) and all(abs(p[0] - q[0]) <= 0.5 and abs(p[1] - q[1]) <= 0.5
                                    for s, r in zip(a, b) for p, q in zip(s, r))

assert not same(frames[0], frames[1]), 'frame at t=1 equals t=0: the scene must depend on t'
assert same(frames[0], frames[8]), 'scene must be periodic: t=0 and t=8 differ'
print('PASS: self-contained SVG, visible faces, Lambert shades, t-driven deterministic frames, period 8 s')
'''

SCENARIO = dict(prompt=PROMPT, spec_text=SPEC, check=CHECK, validation_timeout=120, timeout_seconds=600,
                visual=dict(page='index.html', times=CAPTURE_TIMES, width=800, height=600))


def find_chrome():
    for name in ('CHROME', 'CHROME_PATH'):
        if os.environ.get(name) and Path(os.environ[name]).exists():
            return os.environ[name]
    for path in CHROME_CANDIDATES:
        if Path(path).exists():
            return path
    for name in CHROME_NAMES:
        if shutil.which(name):
            return shutil.which(name)
    return None


def capture(workspace, out_dir, arm, spec):
    """Screenshots and DOM stats of the arm's page at fixed t; never fails the arm."""
    out_dir.mkdir(parents=True, exist_ok=True)
    page = Path(workspace) / spec['page']
    record = {'arm': arm, 'page': spec['page'], 'exists': page.is_file(), 'frames': []}
    chrome = find_chrome()
    record['browser'] = chrome
    if not page.is_file() or not chrome:
        (out_dir / f'{arm}.json').write_text(json.dumps(record, indent=2), encoding='utf-8')
        return record
    record['bytes'] = page.stat().st_size
    record['lines'] = page.read_text(encoding='utf-8', errors='replace').count('\n') + 1
    shutil.copy2(page, out_dir / f'{arm}.{page.name}')
    for t in spec['times']:
        shot = out_dir / f'{arm}-t{t}.png'
        url = page.resolve().as_uri() + f'?t={t}'
        frame = {'t': t, 'png': shot.name}
        with tempfile.TemporaryDirectory(prefix='bench-chrome-') as profile:
            common = [chrome, '--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
                      '--disable-extensions', '--hide-scrollbars', '--user-data-dir=' + profile,
                      '--virtual-time-budget=1000', f"--window-size={spec['width']},{spec['height']}"]
            try:
                subprocess.run([*common, '--screenshot=' + str(shot), url], capture_output=True, timeout=60)
                dom = subprocess.run([*common, '--dump-dom', url], capture_output=True, text=True, timeout=60).stdout
            except (OSError, subprocess.SubprocessError) as error:
                frame['error'] = str(error)
                record['frames'].append(frame)
                continue
        frame['png_ok'] = shot.exists() and shot.stat().st_size > 0
        frame['faces'] = dom.count('class="face"')
        fills = set()
        for chunk in dom.split('<polygon')[1:]:
            tag = chunk.split('>', 1)[0]
            if 'face' in tag:
                for attribute in ('fill="', 'style="'):
                    if attribute in tag:
                        fills.add(tag.split(attribute, 1)[1].split('"', 1)[0])
                        break
        frame['distinct_fills'] = len(fills)
        for key in ('data-faces', 'data-angle'):
            marker = key + '="'
            body = dom.split('<body', 1)[1].split('>', 1)[0] if '<body' in dom else ''
            frame[key] = body.split(marker, 1)[1].split('"', 1)[0] if marker in body else None
        record['frames'].append(frame)
    (out_dir / f'{arm}.json').write_text(json.dumps(record, indent=2), encoding='utf-8')
    return record


def gallery(campaign, arms, spec):
    """Side-by-side page: live animation per arm plus the fixed-t screenshots."""
    out_dir = campaign / 'visual'
    records = {}
    for arm in arms:
        path = out_dir / f'{arm}.json'
        if path.exists():
            records[arm] = json.loads(path.read_text(encoding='utf-8'))
    columns = []
    for arm in arms:
        record = records.get(arm) or {}
        live = f'{arm}.{spec["page"]}'
        parts = [f'<h2>{html.escape(arm)}</h2>']
        if (out_dir / live).exists():
            parts.append(f'<div class="live"><iframe src="visual/{html.escape(live)}" width="{spec["width"]}" height="{spec["height"]}" title="{html.escape(arm)} live"></iframe></div>')
        else:
            parts.append('<p>página não produzida</p>')
        for frame in record.get('frames', []):
            stats = f"t={frame['t']}s · faces={frame.get('faces')} · tons={frame.get('distinct_fills')} · ângulo={frame.get('data-angle')}"
            parts.append(f'<figure><img src="visual/{html.escape(frame["png"])}" alt="{html.escape(arm)} t={frame["t"]}">'
                         f'<figcaption>{html.escape(stats)}</figcaption></figure>')
        meta = f"{record.get('bytes', '-')} bytes · {record.get('lines', '-')} linhas"
        parts.append(f'<p class="meta">{html.escape(meta)}</p>')
        columns.append('<section>' + ''.join(parts) + '</section>')
    document = ('<!doctype html><meta charset="utf-8"><title>Comparativo visual</title>'
                '<style>body{font:14px system-ui;margin:16px;background:#111;color:#eee}'
                'main{display:flex;gap:24px;align-items:flex-start}section{flex:1;min-width:0}'
                '.live{overflow:hidden;border:1px solid #444}'
                'iframe{border:0;display:block;transform-origin:0 0}'
                'figure{display:inline-block;width:calc(50% - 12px);margin:6px 6px 0 0}'
                'img{width:100%;border:1px solid #333}figcaption{font-size:12px;color:#bbb}'
                '.meta{color:#aaa}</style>'
                # Scale each fixed-size live page to its column so both arms fit side by side.
                '<script>function fit(){for(const box of document.querySelectorAll(".live")){'
                'const frame=box.firstElementChild,k=box.clientWidth/frame.width;'
                'frame.style.transform="scale("+k+")";box.style.height=frame.height*k+"px"}}'
                'addEventListener("load",fit);addEventListener("resize",fit)</script>'
                f'<h1>{html.escape(campaign.name)} — comparativo visual</h1>'
                '<p>Ao vivo: animação sem <code>?t</code>. Quadros: mesmos instantes t para os dois braços.</p>'
                '<main>' + ''.join(columns) + '</main>')
    (campaign / 'visual.html').write_text(document, encoding='utf-8')
    return campaign / 'visual.html'
