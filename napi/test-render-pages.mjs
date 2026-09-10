import { readFileSync } from 'node:fs';
import { strict as assert } from 'node:assert';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { renderPdfPages } from './index.js';

const here = dirname(fileURLToPath(import.meta.url));
const dtsPath = join(here, 'index.d.ts');

const PNG_SIGNATURE = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);

function buildPdf(objects, rootId) {
  const header = Buffer.from('%PDF-1.7\n', 'utf8');
  const parts = [header];
  const offsets = new Map();
  let offset = header.length;
  const ids = Object.keys(objects).map(Number).sort((a, b) => a - b);
  for (const id of ids) {
    const head = Buffer.from(`${id} 0 obj\n`, 'utf8');
    const body = Buffer.from(`${objects[id]}\n`, 'utf8');
    const tail = Buffer.from('endobj\n', 'utf8');
    offsets.set(id, offset);
    parts.push(head, body, tail);
    offset += head.length + body.length + tail.length;
  }
  const xrefOffset = offset;
  const size = Math.max(...ids) + 1;
  let xref = `xref\n0 ${size}\n`;
  for (let i = 0; i < size; i += 1) {
    if (offsets.has(i)) {
      xref += `${String(offsets.get(i)).padStart(10, '0')} 00000 n \n`;
    } else {
      xref += '0000000000 65535 f \n';
    }
  }
  const trailer = `trailer\n<< /Size ${size} /Root ${rootId} 0 R >>\nstartxref\n${xrefOffset}\n%%EOF`;
  return Buffer.concat([...parts, Buffer.from(xref, 'utf8'), Buffer.from(trailer, 'utf8')]);
}

function blankTwoPagePdf() {
  return buildPdf(
    {
      1: '<< /Type /Catalog /Pages 2 0 R >>',
      2: '<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>',
      3: '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>',
      4: '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>',
    },
    1
  );
}

function pngDimensions(png) {
  assert.ok(png.subarray(0, 8).equals(PNG_SIGNATURE), 'missing PNG signature');
  assert.equal(png.subarray(12, 16).toString('latin1'), 'IHDR');
  return { width: png.readUInt32BE(16), height: png.readUInt32BE(20) };
}

const pdf = blankTwoPagePdf();

// Single page: real PNG, true dimensions, page echo.
{
  const pages = renderPdfPages(pdf, [1]);
  assert.equal(pages.length, 1);
  assert.equal(pages[0].pageNumber, 1);
  assert.ok(pages[0].width > 0 && pages[0].height > 0);
  assert.deepEqual(pngDimensions(pages[0].png), { width: pages[0].width, height: pages[0].height });
}

// Request order kept; duplicates removed deterministically.
{
  const pages = renderPdfPages(pdf, [2, 1]);
  assert.deepEqual(
    pages.map((p) => p.pageNumber),
    [2, 1]
  );
  const deduped = renderPdfPages(pdf, [2, 1, 2, 1]);
  assert.deepEqual(
    deduped.map((p) => p.pageNumber),
    [2, 1]
  );
}

// Lower DPI renders smaller images.
{
  const low = renderPdfPages(pdf, [1], { dpi: 72 });
  const high = renderPdfPages(pdf, [1]);
  assert.ok(low[0].width < high[0].width);
  assert.ok(low[0].png.length < high[0].png.length);
}

// Fail-closed inputs.
assert.throws(() => renderPdfPages(pdf, []), /at least one/);
assert.throws(() => renderPdfPages(pdf, [0]), /1-indexed|invalid/i);
assert.throws(() => renderPdfPages(pdf, [3]), /out of bounds|bounds/i);
assert.throws(() => renderPdfPages(pdf, [1, 2, 3, 4, 5, 6, 7, 8, 9]), /at most|too many/i);
assert.throws(() => renderPdfPages(pdf, [1], { dpi: 0 }), /DPI/i);
assert.throws(() => renderPdfPages(pdf, [1], { dpi: NaN }), /DPI/i);
assert.throws(() => renderPdfPages(pdf, [1], { dpi: 301 }), /DPI|maximum/i);
assert.throws(() => renderPdfPages(Buffer.from('not a pdf'), [1]), /render_pdf_pages/);
assert.throws(() => renderPdfPages(Buffer.alloc(0), [1]), /render_pdf_pages/);

// Generated types carry the new API.
{
  const dts = readFileSync(dtsPath, 'utf8');
  assert.ok(dts.includes('renderPdfPages'));
  assert.ok(dts.includes('RenderedPagePng'));
  assert.ok(dts.includes('RenderPdfPagesOptions'));
  assert.ok(dts.includes('pageNumber'));
}

console.log('test-render-pages: ok');
