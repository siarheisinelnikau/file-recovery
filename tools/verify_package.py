import hashlib
import io
import json
import struct
import sys
from pathlib import Path
from PIL import Image

root = Path(__file__).resolve().parent.parent
path = Path(sys.argv[1]) if len(sys.argv) > 1 else root / "dist" / "FileRecovery-v0.1.0.exe"
data = path.read_bytes()

def u16(at):
    return struct.unpack_from("<H", data, at)[0]

def u32(at):
    return struct.unpack_from("<I", data, at)[0]

pe = u32(60)
assert data[:2] == b"MZ" and data[pe:pe + 4] == b"PE\0\0" and u16(pe + 4) == 0x8664
optional = pe + 24
assert u16(optional) == 0x20B
sections = []
for index in range(u16(pe + 6)):
    at = optional + u16(pe + 20) + index * 40
    sections.append((u32(at + 12), max(u32(at + 8), u32(at + 16)), u32(at + 20)))

def file_offset(rva):
    for start, size, offset in sections:
        if start <= rva < start + size:
            return offset + rva - start
    raise AssertionError(f"Invalid RVA: {rva}")

resource = file_offset(u32(optional + 112 + 16))
leaves = {}

def directory(relative, trail=()):
    assert len(trail) <= 3
    base = resource + relative
    count = u16(base + 12) + u16(base + 14)
    assert count < 1024
    for index in range(count):
        at = base + 16 + index * 8
        name, target = u32(at), u32(at + 4)
        assert not name & 0x80000000
        key = trail + (name,)
        if target & 0x80000000:
            directory(target & 0x7FFFFFFF, key)
        else:
            entry = resource + target
            offset, size = file_offset(u32(entry)), u32(entry + 4)
            leaves[key] = data[offset:offset + size]

directory(0)
group = next(payload for key, payload in leaves.items() if key[:2] == (14, 1))
assert group[:4] == b"\0\0\1\0"
count = struct.unpack_from("<H", group, 4)[0]
ico_header = bytearray(group[:6])
payloads = bytearray()
sizes = []
for index in range(count):
    width, height, colors, reserved, planes, bits, size, resource_id = struct.unpack_from("<BBBBHHIH", group, 6 + index * 14)
    payload = next(p for key, p in leaves.items() if key[:2] == (3, resource_id))
    assert len(payload) == size
    ico_header.extend(struct.pack("<BBBBHHII", width, height, colors, reserved, planes, bits, size, 6 + 16 * count + len(payloads)))
    payloads.extend(payload)
    sizes.append((width or 256, height or 256))
icon = Image.open(io.BytesIO(ico_header + payloads))
for size in sizes:
    image = icon.ico.getimage(size).convert("RGBA")
    bounds = image.getchannel("A").getbbox()
    assert bounds and bounds[2] - bounds[0] >= size[0] * .7 and bounds[3] - bounds[1] >= size[1] * .7
    assert len(image.getcolors(size[0] * size[1]) or []) > 8
    image.save(root / "target" / f"icon-proof-{size[0]}.png")
assert {(16,16),(20,20),(24,24),(32,32),(48,48),(64,64),(128,128),(256,256)} <= set(sizes)
assert any(b'asInvoker' in p for k,p in leaves.items() if k[0] == 24)
imports = []
descriptor = file_offset(u32(optional + 112 + 8))
while any(data[descriptor:descriptor + 20]):
    name = file_offset(u32(descriptor + 12))
    end = data.index(b"\0", name)
    imports.append(data[name:end].decode("ascii").lower())
    descriptor += 20
assert not any(name.startswith(("d3d", "dxgi", "opengl", "vulkan", "vcruntime", "msvcp")) for name in imports)
digest = hashlib.sha256(data).hexdigest()
print(json.dumps({"artifact":str(path),"bytes":len(data),"sha256":digest,"embedded_icon_sizes":sizes,"imports":imports,"manifest":"asInvoker"}, indent=2))
