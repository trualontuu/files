# Логотип Files: папка из трёх серых слоёв (тёмный / средний / светлый) — родня логотипу Mono.
from PIL import Image, ImageDraw
S = 1024
def layer(d, y0, h, col, tab):
    x0, x1 = 120, 904
    r = 70
    if tab:
        d.rounded_rectangle([x0, y0 - 90, x0 + 330, y0 + 60], radius=60, fill=col)
    d.rounded_rectangle([x0, y0, x1, y0 + h], radius=r, fill=col)
def render(gap):
    im = Image.new('RGBA', (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    layer(d, 250, 520, (74, 74, 74, 255), True)
    layer(d, 400, 420, (138, 138, 138, 255), False)
    layer(d, 560, 330, (237, 237, 237, 255), False)
    # щели между слоями
    cut = Image.new('L', (S, S), 0); cd = ImageDraw.Draw(cut)
    for y in (400, 560):
        cd.rounded_rectangle([100, y - gap, 924, y], radius=0, fill=255)
    im.putalpha(Image.composite(Image.new('L', (S, S), 0), im.getchannel('A'), cut))
    return im
big = render(26)
small = render(60)
sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
imgs = [(small if s <= 32 else big).resize((s, s), Image.LANCZOS) for s in sizes]
imgs[-1].save('files.ico', sizes=[(s, s) for s in sizes], append_images=imgs[:-1])
big.resize((512, 512), Image.LANCZOS).save('files.png')
