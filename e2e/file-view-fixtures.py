"""Generate small real spreadsheet inputs without a package/network dependency."""
import html
import pathlib
import sys
import zipfile

root = pathlib.Path(sys.argv[1])
end = sys.argv[2]
values = ["Evidence row " + str(i) for i in range(150)] + [end]
rows = "".join(
    '<row r="{n}"><c r="A{n}" t="inlineStr"><is><t>{text}</t></is></c></row>'.format(
        n=i + 1, text=html.escape(text)
    )
    for i, text in enumerate(values)
)
with zipfile.ZipFile(root / "workbook.xlsx", "w", zipfile.ZIP_DEFLATED) as book:
    book.writestr("[Content_Types].xml", '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>')
    book.writestr("_rels/.rels", '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>')
    book.writestr("xl/workbook.xml", '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Evidence" sheetId="1" r:id="rId1"/></sheets></workbook>')
    book.writestr("xl/_rels/workbook.xml.rels", '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>')
    book.writestr("xl/worksheets/sheet1.xml", '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:A151"/><cols><col min="1" max="1" width="45" customWidth="1"/></cols><sheetData>' + rows + '</sheetData></worksheet>')

ods_rows = "".join('<table:table-row><table:table-cell office:value-type="string"><text:p>' + html.escape(value) + '</text:p></table:table-cell></table:table-row>' for value in values)
with zipfile.ZipFile(root / "workbook.ods", "w") as book:
    book.writestr("mimetype", "application/vnd.oasis.opendocument.spreadsheet")
    book.writestr("content.xml", '<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:version="1.2"><office:body><office:spreadsheet><table:table table:name="Evidence">' + ods_rows + '</table:table></office:spreadsheet></office:body></office:document-content>')
    book.writestr("META-INF/manifest.xml", '<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/></manifest:manifest>')
