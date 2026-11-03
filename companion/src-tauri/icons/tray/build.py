# Regenerates both tray icon sets (the template PNGs here and win/*.ico) from the Keo K path
# (KPATH below) with ImageMagick (`magick`). Run from this folder: python3 build.py
# Working files go to a temporary folder; only *.png here and win/*.ico are written.
import subprocess, os, shutil, tempfile
HERE = os.path.dirname(os.path.abspath(__file__))
WORK = tempfile.mkdtemp(prefix='keo-tray-')
os.makedirs(os.path.join(HERE, 'win'), exist_ok=True)
os.chdir(WORK)
os.makedirs('mac', exist_ok=True)
os.makedirs('win', exist_ok=True)
# The K from the Keo Slack app icon, in its own path units.
KPATH = (
    'M 762.936 584.201 C 765.766 584.013 768.653 583.817 771.519 583.819 C 790.69 582.823 814.217 602.132 816.603 621.397 C 819.479 644.628 818.181 673.838 818.181 697.719 L 818.071 856.713 C 850.447 822.606 882.495 788.189 914.209 753.466 C 928.549 737.92 942.936 722.388 957.395 707.029 C 975.033 688.292 1005.55 686.394 1023.91 705.076 C 1034.65 715.998 1040.91 736.069 1031.16 749.483 C 1019.89 765.01 1005.4 778.498 992.682 792.858 C 973.836 812.593 955.598 833.941 936.156 853.045 C 946.613 866.311 963.424 884.076 975.132 896.936 L 1039.95 967.984 C 1049.88 978.836 1071.76 1000.7 1078.34 1012.33 C 1088.1 1039.71 1077.46 1069.39 1046.87 1076.94 C 1030.6 1080.96 1015.22 1073.1 1004.08 1061.49 C 990.439 1047.27 977.147 1032.19 963.78 1017.32 L 904.228 951.111 C 900.209 946.629 877.304 919.139 873.912 922.103 C 861.669 932.8 826.411 967.738 819.254 980.515 C 817.969 1015.15 830.014 1062.29 786.542 1075.39 C 774.244 1079.13 760.954 1077.7 749.733 1071.43 C 717.482 1053.53 728.059 1015.25 726.173 984.631 C 725.793 978.461 726.172 971.448 726.172 965.2 L 726.153 836.08 L 726.056 700.772 C 726.032 677.175 725.642 653.202 726.323 629.626 C 726.991 606.523 740.759 590.26 762.936 584.201 z'
)
# K bbox in path units
BX, BY, BW, BH = 725.5, 583.5, 357.0, 495.5

def k_svg(size, kh, color, opacity=1.0, extra_defs=''):
    s = kh / BH
    kw = BW * s
    tx = (size - kw) / 2 - BX * s
    ty = (size - kh) / 2 - BY * s
    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="0 0 {size} {size}">'
            f'{extra_defs}<g transform="translate({tx:.4f},{ty:.4f}) scale({s:.6f})">'
            f'<path fill="{color}" fill-opacity="{opacity}" d="{KPATH}"/></g></svg>')

def render(svg, out, size):
    open('tmp.svg','w').write(svg)
    subprocess.run(['magick','-background','none','-density','384','tmp.svg','-resize',f'{size}x{size}!',out],check=True)

def badge(png, size, cx, cy, r, glyph, fill='black', glyph_color=None, knock=True):
    """Punch a hole under the badge, draw the badge circle, cut the glyph out of it."""
    cmd = ['magick', png]
    if knock:
        cmd += ['(', '-size', f'{size}x{size}', 'xc:none', '-fill', 'black', '-draw', f'circle {cx},{cy} {cx+r+1.6},{cy}', ')', '-compose', 'DstOut', '-composite']
    cmd += ['(', '-size', f'{size}x{size}', 'xc:none', '-fill', fill, '-draw', f'circle {cx},{cy} {cx+r},{cy}', ')', '-compose', 'Over', '-composite']
    if glyph:
        if glyph_color:  # draw glyph in colour on top
            cmd += ['(', '-size', f'{size}x{size}', 'xc:none', '-fill', glyph_color, '-stroke', glyph_color] + glyph + [')', '-compose', 'Over', '-composite']
        else:            # cut glyph out (template: transparent)
            cmd += ['(', '-size', f'{size}x{size}', 'xc:none', '-fill', 'black', '-stroke', 'black'] + glyph + [')', '-compose', 'DstOut', '-composite']
    cmd += [png]
    subprocess.run(cmd, check=True)

def glyphs(cx, cy, r):
    u = r / 7.0
    return {
      'paused': ['-strokewidth', f'{1.9*u:.2f}', '-draw', f'line {cx-1.7*u:.2f},{cy-3*u:.2f} {cx-1.7*u:.2f},{cy+3*u:.2f}', '-draw', f'line {cx+1.7*u:.2f},{cy-3*u:.2f} {cx+1.7*u:.2f},{cy+3*u:.2f}'],
      'needs-attention': ['-strokewidth', f'{2.0*u:.2f}', '-draw', f'line {cx},{cy-3.6*u:.2f} {cx},{cy+0.6*u:.2f}', '-strokewidth', '0', '-draw', f'circle {cx},{cy+3.2*u:.2f} {cx+1.15*u:.2f},{cy+3.2*u:.2f}'],
      'needs-update': ['-strokewidth', f'{1.9*u:.2f}', '-draw', f'line {cx},{cy-3.4*u:.2f} {cx},{cy+2.8*u:.2f}', '-draw', f'line {cx-3*u:.2f},{cy+0.1*u:.2f} {cx},{cy+3.1*u:.2f}', '-draw', f'line {cx+3*u:.2f},{cy+0.1*u:.2f} {cx},{cy+3.1*u:.2f}'],
      'working': ['-strokewidth', '0', '-draw', f'circle {cx},{cy} {cx+2.6*u:.2f},{cy}'],
    }

# ---- macOS template set: 44x44 (@2x of 22pt), black + alpha ----
SIZE, KH = 44, 34
cx, cy, r = 35.5, 34.5, 7.5
G = glyphs(cx, cy, r)
for state in ['signed-out','listening','paused','working','needs-attention','needs-update']:
    out = f'mac/{state}.png'
    op = 0.42 if state == 'signed-out' else 1.0
    render(k_svg(SIZE, KH, '#000', op), out, SIZE)
    if state in G:
        badge(out, SIZE, cx, cy, r, G[state])
# 1x versions (22px) for reference / future @1x use
for state in ['signed-out','listening','paused','working','needs-attention','needs-update']:
    subprocess.run(['magick', f'mac/{state}.png', '-resize', '22x22', f'mac/{state}@1x.png'], check=True)
# Tauri's PNG loader wants 8-bit RGBA: the shipped copies sit next to this file.
for state in ['signed-out','listening','paused','working','needs-attention','needs-update']:
    subprocess.run(['magick', f'mac/{state}.png', f'PNG32:{HERE}/{state}.png'], check=True)

# ---- Windows colour set: gradient K on transparent, 32 + 16, badge as a coloured dot ----
GRAD = ('<defs><linearGradient id="ki" gradientUnits="userSpaceOnUse" x1="717" y1="1100" x2="1100" y2="560">'
        '<stop offset="0" stop-color="#b8f0c9"/><stop offset="0.38" stop-color="#7fee64"/>'
        '<stop offset="0.74" stop-color="#45dcc5"/><stop offset="1" stop-color="#7cd9f0"/></linearGradient></defs>')
WIN = {'signed-out': ('#9a9a9a', None), 'listening': (None, None), 'paused': ('#f2b134', 'paused'),
       'working': ('#45dcc5', 'working'), 'needs-attention': ('#ff5a5f', 'needs-attention'), 'needs-update': ('#f2b134', 'needs-update')}
for size in (32, 16):
    kh = 26 if size == 32 else 13
    cx, cy, r = (25.5, 25.5, 5.5) if size == 32 else (12.5, 12.5, 3.0)
    G = glyphs(cx, cy, r)
    for state, (dot, glyph) in WIN.items():
        out = f'win/{state}-{size}.png'
        if state == 'signed-out':
            render(k_svg(size, kh, '#9a9a9a', 0.9), out, size)
        else:
            render(k_svg(size, kh, '#000', 1.0), out, size)
            m = size - 1
            subprocess.run(['magick', '-size', f'{size}x{size}', 'xc:', '-sparse-color', 'Inverse',
                            f'0,{m} #b8f0c9 {0.38*m:.1f},{(1-0.38)*m:.1f} #7fee64 {0.74*m:.1f},{(1-0.74)*m:.1f} #45dcc5 {m},0 #7cd9f0',
                            '(', out, '-alpha', 'extract', ')', '-compose', 'CopyOpacity', '-composite', out], check=True)
        if glyph:
            badge(out, size, cx, cy, r, G[glyph], fill=dot, glyph_color='#0f1011')
for state in WIN:
    subprocess.run(['magick', f'win/{state}-16.png', f'win/{state}-32.png', f'{HERE}/win/{state}.ico'], check=True)
shutil.rmtree(WORK)
print('done')
