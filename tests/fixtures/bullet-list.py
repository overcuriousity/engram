#!/usr/bin/env python3
"""Build tests/fixtures/bullet-list.pdf.

Hand-built rather than converted, so that the one detail under test is
guaranteed present: the bullet of a symbol font, whose ToUnicode CMap maps it
to the private-use codepoint U+F0B7 -- exactly what Word and LibreOffice put
in a PDF for a bulleted list, and what no font on the reading side renders.
"""
import sys, zlib

def esc(s):
    return s.replace("\\", r"\\").replace("(", r"\(").replace(")", r"\)")

lines = []
def text(font, size, x, y, s):
    lines.append(f"BT /{font} {size} Tf 1 0 0 1 {x} {y} Tm ({esc(s)}) Tj ET")

y = 760
text("F1", 12, 72, y, "The quarterly plan lists three goals.")
y -= 40
text("F1", 12, 72, y, "Each of them is owned by one team.")
y -= 40
for item in ["ship the extraction door",
             "measure what it costs",
             "write down what it cannot do"]:
    text("F2", 12, 72, y, "\xb7")          # symbol-font bullet
    text("F1", 12, 90, y, item)
    y -= 24
y -= 20
text("F1", 12, 72, y, "Nothing else is in scope this quarter.")

content = "\n".join(lines).encode("latin-1")

tounicode = b"""/CIDInit /ProcSet findresource begin
12 dict begin
begincmap
/CMapName /Symbol-bullet def
/CMapType 2 def
1 begincodespacerange
<00> <ff>
endcodespacerange
1 beginbfchar
<b7> <f0b7>
endbfchar
endcmap
CMapName currentdict /CMap defineresource pop
end
end
"""

objs = {}
objs[1] = b"<< /Type /Catalog /Pages 2 0 R >>"
objs[2] = b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>"
objs[3] = (b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
           b"/Resources << /Font << /F1 5 0 R /F2 6 0 R >> >> /Contents 4 0 R >>")
objs[4] = b"<< /Length %d >>\nstream\n" % len(content) + content + b"\nendstream"
objs[5] = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
objs[6] = (b"<< /Type /Font /Subtype /Type1 /BaseFont /Symbol /FirstChar 183 /LastChar 183 "
           b"/Widths [350] /ToUnicode 7 0 R >>")
objs[7] = b"<< /Length %d >>\nstream\n" % len(tounicode) + tounicode + b"\nendstream"

out = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
offsets = {}
for n in sorted(objs):
    offsets[n] = len(out)
    out += b"%d 0 obj\n" % n + objs[n] + b"\nendobj\n"
xref = len(out)
out += b"xref\n0 %d\n" % (len(objs) + 1)
out += b"0000000000 65535 f \n"
for n in sorted(objs):
    out += b"%010d 00000 n \n" % offsets[n]
out += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(objs) + 1, xref)

open(sys.argv[1], "wb").write(bytes(out))
print("wrote", sys.argv[1], len(out), "bytes")
