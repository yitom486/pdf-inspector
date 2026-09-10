import { readFileSync } from 'node:fs';
import { strict as assert } from 'node:assert';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { extractEmbeddedOutline } from './index.js';

const here = dirname(fileURLToPath(import.meta.url));
const fixturePath = join(here, '..', 'tests', 'fixtures', 'thermo-freon12.pdf');
const dtsPath = join(here, 'index.d.ts');

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

function twoPageBase() {
  return {
    1: '<< /Type /Pages /Kids [2 0 R 3 0 R] /Count 2 >>',
    2: '<< /Type /Page /Parent 1 0 R /MediaBox [0 0 612 792] >>',
    3: '<< /Type /Page /Parent 1 0 R /MediaBox [0 0 612 792] >>',
  };
}

function deepChainPdf(depth) {
  const objects = {
    1: '<< /Type /Pages /Kids [2 0 R] /Count 1 >>',
    2: '<< /Type /Page /Parent 1 0 R /MediaBox [0 0 612 792] >>',
    4: '<< /Type /Catalog /Pages 1 0 R /Outlines 5 0 R >>',
  };
  let child = null;
  let nextId = 6;
  for (let i = depth - 1; i >= 0; i -= 1) {
    const id = nextId;
    nextId += 1;
    let body = `<< /Title (Level ${i}) /Dest [2 0 R /Fit]`;
    if (child !== null) {
      body += ` /First ${child} 0 R`;
    }
    body += ' >>';
    objects[id] = body;
    child = id;
  }
  objects[5] = `<< /First ${child} 0 R /Last ${child} 0 R /Count 1 >>`;
  return buildPdf(objects, 4);
}

const emptyFixture = readFileSync(fixturePath);
const emptyResult = extractEmbeddedOutline(emptyFixture);
assert.deepEqual(emptyResult.items, []);
assert.equal(emptyResult.unresolvedCount, 0);
assert.equal(emptyResult.truncated, false);

{
  const objects = {
    ...twoPageBase(),
    4: '<< /Type /Catalog /Pages 1 0 R /Outlines 5 0 R >>',
    5: '<< /First 6 0 R /Last 7 0 R /Count 2 >>',
    6: '<< /Title (Chapter 1) /Dest [2 0 R /Fit] /Next 7 0 R /First 8 0 R >>',
    8: '<< /Title (Section 1.1) /Dest [3 0 R /Fit] >>',
    7: '<< /Title (Chapter 2) /Dest [3 0 R /Fit] >>',
  };
  const pdf = buildPdf(objects, 4);
  const result = extractEmbeddedOutline(pdf);
  assert.equal(result.items.length, 3);
  assert.equal(result.items[0].title, 'Chapter 1');
  assert.equal(result.items[0].level, 1);
  assert.equal(result.items[0].physicalPage, 1);
  assert.equal(result.items[1].title, 'Section 1.1');
  assert.equal(result.items[1].level, 2);
  assert.equal(result.items[1].physicalPage, 2);
  assert.equal(result.items[2].title, 'Chapter 2');
  assert.equal(result.items[2].level, 1);
  assert.equal(result.items[2].physicalPage, 2);
  assert.equal(result.unresolvedCount, 0);
  assert.equal(result.truncated, false);
}

{
  const objects = {
    ...twoPageBase(),
    4: '<< /Type /Catalog /Pages 1 0 R /Names 5 0 R /Outlines 7 0 R >>',
    5: '<< /Dests 6 0 R >>',
    6: '<< /Names [(chap2) [3 0 R /Fit]] >>',
    7: '<< /First 8 0 R /Last 8 0 R /Count 1 >>',
    8: '<< /Title (Named chapter) /Dest (chap2) >>',
  };
  const pdf = buildPdf(objects, 4);
  const result = extractEmbeddedOutline(pdf);
  assert.equal(result.items.length, 1);
  assert.equal(result.items[0].title, 'Named chapter');
  assert.equal(result.items[0].physicalPage, 2);
  assert.equal(result.unresolvedCount, 0);
  assert.equal(result.truncated, false);
}

{
  const objects = {
    ...twoPageBase(),
    4: '<< /Type /Catalog /Pages 1 0 R /Outlines 5 0 R >>',
    5: '<< /First 6 0 R /Last 6 0 R /Count 1 >>',
    6: '<< /Title (Broken) /Dest [99 0 R /Fit] >>',
  };
  const pdf = buildPdf(objects, 4);
  const result = extractEmbeddedOutline(pdf);
  assert.equal(result.items.length, 1);
  assert.equal(result.items[0].title, 'Broken');
  assert.ok(result.items[0].physicalPage == null);
  assert.equal(result.unresolvedCount, 1);
  assert.equal(result.truncated, false);
}

{
  const kinds = [
    '<< /Title (Remote) /A << /S /GoToR /F (other.pdf) /D [3 0 R /Fit] >> >>',
    '<< /Title (Web) /A << /S /URI /URI (https://example.com/secret) >> >>',
    '<< /Title (Code) /A << /S /JavaScript /JS (app.alert(1)) >> >>',
  ];
  for (const body of kinds) {
    const objects = {
      ...twoPageBase(),
      4: '<< /Type /Catalog /Pages 1 0 R /Outlines 5 0 R >>',
      5: '<< /First 6 0 R /Last 6 0 R /Count 1 >>',
      6: body,
    };
    const pdf = buildPdf(objects, 4);
    const result = extractEmbeddedOutline(pdf);
    assert.equal(result.items.length, 1);
    assert.ok(result.items[0].physicalPage == null);
    assert.equal(result.unresolvedCount, 1);
    assert.equal(result.truncated, false);
    const serialised = JSON.stringify(result.items[0]);
    assert.ok(!serialised.includes('example.com'));
    assert.ok(!serialised.includes('other.pdf'));
    assert.ok(!serialised.includes('app.alert'));
  }
}

{
  const objects = {
    ...twoPageBase(),
    4: '<< /Type /Catalog /Pages 1 0 R /Outlines 5 0 R >>',
    5: '<< /First 6 0 R /Last 6 0 R /Count 1 >>',
    6: '<< /Title (One) /Dest [2 0 R /Fit] /Next 7 0 R >>',
    7: '<< /Title (Two) /Dest [2 0 R /Fit] /Next 6 0 R >>',
  };
  const pdf = buildPdf(objects, 4);
  const result = extractEmbeddedOutline(pdf);
  assert.equal(result.items.length, 2);
  assert.equal(result.items[0].title, 'One');
  assert.equal(result.items[1].title, 'Two');
  assert.equal(result.truncated, false);
}

{
  const pdf = deepChainPdf(40);
  const result = extractEmbeddedOutline(pdf);
  assert.equal(result.items.length, 32);
  assert.equal(result.truncated, true);
}

assert.throws(() => extractEmbeddedOutline(Buffer.from('not a pdf')), /extract_embedded_outline/);
assert.throws(() => extractEmbeddedOutline(Buffer.from('')), /extract_embedded_outline/);

{
  const dts = readFileSync(dtsPath, 'utf8');
  assert.ok(dts.includes('extractEmbeddedOutline'));
  assert.ok(dts.includes('EmbeddedOutline'));
  assert.ok(dts.includes('EmbeddedOutlineItem'));
  assert.ok(dts.includes('physicalPage'));
  assert.ok(dts.includes('unresolvedCount'));
  assert.ok(dts.includes('truncated'));
}
