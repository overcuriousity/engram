#!/usr/bin/env python3
"""Build tests/fixtures/identity-h.pdf.

Hand-built rather than converted, so that the one detail under test is
guaranteed present: every string on the page is two bytes per glyph -- a
Type0 font with `/Encoding /Identity-H`, the way Word, LibreOffice and every
browser embed a TrueType subset -- and the ToUnicode CMap maps those 2-byte
CIDs back to text, the `fl` ligature again through a multi-unit bfrange. Read
a byte at a time, the words would come apart into single letters.
"""
import sys

LINES = ["Du hast eine Datei an Ronny gesendet.",
         "Forensik und deren Begrifflichkeiten",
         "Jede Spur wird gesichert, bevor sie gedeutet wird."]
FL = 0x0300  # the ligature glyph's CID

def glyphs(s):
    """Split a line into glyphs: `fl` is one."""
    out, i = [], 0
    while i < len(s):
        if s.startswith("fl", i):
            out.append("fl")
            i += 2
        else:
            out.append(s[i])
            i += 1
    return out

chars = sorted({g for s in LINES for g in glyphs(s) if g != "fl"})
cid = {c: 0x0100 + n for n, c in enumerate(chars)}  # high byte non-zero on purpose
cid["fl"] = FL

lines = []
y = 740
for s in LINES:
    hexs = "".join("%04X" % cid[g] for g in glyphs(s)).encode()
    lines.append(b"BT /F1 12 Tf 1 0 0 1 72 %d Tm <" % y + hexs + b"> Tj ET")
    y -= 24
content = b"\n".join(lines)

bfchar = b"\n".join(b"<%04X> <%04X>" % (cid[c], ord(c)) for c in chars)
tounicode = (b"""/CIDInit /ProcSet findresource begin
12 dict begin
begincmap
/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def
/CMapName /Adobe-Identity-UCS def
/CMapType 2 def
1 begincodespacerange
<0000> <FFFF>
endcodespacerange
%d beginbfchar
""" % len(chars) + bfchar + b"""
endbfchar
1 beginbfrange
<%04X> <%04X> <0066006C>
endbfrange
endcmap
CMapName currentdict /CMap defineresource pop
end
end
""" % (FL, FL))

objs = {}
objs[1] = b"<< /Type /Catalog /Pages 2 0 R >>"
objs[2] = b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>"
objs[3] = (b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
           b"/Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>")
objs[4] = b"<< /Length %d >>\nstream\n" % len(content) + content + b"\nendstream"
objs[5] = (b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+Calibri "
           b"/Encoding /Identity-H /DescendantFonts [6 0 R] /ToUnicode 7 0 R >>")
objs[6] = (b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+Calibri "
           b"/CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> "
           b"/DW 500 /CIDToGIDMap /Identity >>")
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
