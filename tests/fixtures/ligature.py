#!/usr/bin/env python3
"""Build tests/fixtures/ligature.pdf.

Hand-built rather than converted, so that the one detail under test is
guaranteed present: a subset font that draws `fl` and `fi` as single ligature
glyphs, and whose ToUnicode CMap maps each of them back to its two letters in
a *bfrange* -- `<1E> <1E> <0066006C>` -- the form subsetting producers use and
the one docling 1.53.0 dropped (engram#60: `Begrifflichkeiten` came out
without its `fl`). Every other code is WinAnsi.
"""
import sys

FL, FI = 0x1E, 0x1F

def run(s):
    """One line of text as a PDF string, `fl`/`fi` drawn as the ligature glyph."""
    out = s.replace("fl", chr(FL)).replace("fi", chr(FI))
    esc = "".join(f"\\{ord(c):03o}" if ord(c) < 32 or c in "()\\" else c for c in out)
    return esc.encode("latin-1")

lines = []
y = 740
for s in ["Forensik und deren Begrifflichkeiten",
          "Die Definition einer Spur steht am Anfang.",
          "Jeder Begriff wird hier einmal erklaert."]:
    lines.append(b"BT /F1 12 Tf 1 0 0 1 72 %d Tm (" % y + run(s) + b") Tj ET")
    y -= 24
content = b"\n".join(lines)

tounicode = b"""/CIDInit /ProcSet findresource begin
12 dict begin
begincmap
/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def
/CMapName /Adobe-Identity-UCS def
/CMapType 2 def
1 begincodespacerange
<00> <FF>
endcodespacerange
2 beginbfrange
<1E> <1E> <0066006C>
<1F> <1F> <00660069>
endbfrange
endcmap
CMapName currentdict /CMap defineresource pop
end
end
"""

first, last = FL, 255
objs = {}
objs[1] = b"<< /Type /Catalog /Pages 2 0 R >>"
objs[2] = b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>"
objs[3] = (b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
           b"/Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>")
objs[4] = b"<< /Length %d >>\nstream\n" % len(content) + content + b"\nendstream"
objs[5] = (b"<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+Calibri "
           b"/FirstChar %d /LastChar %d /Widths [" % (first, last)
           + b" ".join([b"500"] * (last - first + 1))
           + b"] /Encoding /WinAnsiEncoding /ToUnicode 6 0 R >>")
objs[6] = b"<< /Length %d >>\nstream\n" % len(tounicode) + tounicode + b"\nendstream"

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
