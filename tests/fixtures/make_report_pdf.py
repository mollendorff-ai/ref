"""Build tests/fixtures/report.pdf, the PDF behind ref's PDF layout tests.

Run from the repository root: python3 tests/fixtures/make_report_pdf.py
Needs reportlab (pip install reportlab). The PDF is committed, so tests do not
need Python; rerun this only to change the fixture.

What it covers, as generated reports often lay them out:
- the first line is a heading ("1. Introduction"), with no title above it
- table cells drawn at separate positions, numbers right-aligned
- a blank line's worth of space between table rows
- a heading drawn as two runs, number and title ("2.1" + "Notes on the data")
"""
from reportlab.lib.pagesizes import letter
from reportlab.pdfgen import canvas

c = canvas.Canvas("tests/fixtures/report.pdf", pagesize=letter, invariant=1)
c.setTitle("Regional report")
c.setFont("Helvetica", 11)
y = 740


def line(text, x=72):
    global y
    c.drawString(x, y, text)
    y -= 16


line("1. Introduction")
line("This report covers the third quarter for the four regions of the company.")
y -= 10
line("2. Results")
line("Revenue by region, in thousands of dollars:")
y -= 6
for region, q2, q3, change in [
    ("Region", "Q2", "Q3", "Change"),
    ("North", "140", "150", "7%"),
    ("South", "190", "200", "5%"),
    ("East", "80", "90", "12%"),
    ("West", "60", "64", "6%"),
]:
    c.drawString(72, y, region)
    c.drawRightString(230, y, q2)
    c.drawRightString(300, y, q3)
    c.drawRightString(380, y, change)
    y -= 22  # rows spaced out: pdf-extract writes a blank line between them
y -= 6
c.drawString(72, y, "2.1")
c.drawString(100, y, "Notes on the data")
y -= 16
line("Figures are unaudited. East includes two stores opened in August.")
y -= 10
line("3. Conclusion")
line("Growth was strongest in the East.")
c.save()
