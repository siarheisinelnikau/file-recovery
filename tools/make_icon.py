from pathlib import Path
from PIL import Image, ImageDraw

root = Path(__file__).resolve().parent.parent
size = 1024
image = Image.new("RGBA", (size, size))
draw = ImageDraw.Draw(image)
draw.rounded_rectangle((24, 24, 1000, 1000), radius=240, fill="#087F79")
draw.polygon([(280, 204), (574, 204), (748, 378), (748, 810), (280, 810)], fill="#F0FAF7")
draw.polygon([(574, 204), (574, 378), (748, 378)], fill="#A6D8CD")
draw.rounded_rectangle((342, 454, 598, 492), radius=18, fill="#91BEB4")
draw.rounded_rectangle((342, 540, 540, 578), radius=18, fill="#91BEB4")
draw.arc((444, 556, 842, 946), start=192, end=495, fill="#0B4C49", width=82)
draw.polygon([(412, 622), (578, 608), (465, 770)], fill="#0B4C49")
image.save(root / "assets" / "app.ico", sizes=[(16,16), (20,20), (24,24), (32,32), (48,48), (64,64), (128,128), (256,256)])
