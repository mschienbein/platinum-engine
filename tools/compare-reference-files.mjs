import { open } from 'node:fs/promises';
import { createHash } from 'node:crypto';

async function readBlock(handle, buffer) {
  let bytesRead = 0;
  while (bytesRead < buffer.length) {
    const chunk = await handle.read(buffer, bytesRead, buffer.length - bytesRead, null);
    if (chunk.bytesRead === 0) break;
    bytesRead += chunk.bytesRead;
  }
  return { bytesRead };
}

const [leftPath, rightPath, ...extra] = process.argv.slice(2);
if (!leftPath || !rightPath || extra.length) {
  console.error('usage: node tools/compare-reference-files.mjs LEFT RIGHT');
  process.exitCode = 2;
} else {
  const handles = [];
  try {
    const left = await open(leftPath, 'r');
    handles.push(left);
    const right = await open(rightPath, 'r');
    handles.push(right);
    const leftHash = createHash('sha256');
    const rightHash = createHash('sha256');
    const leftBuffer = Buffer.allocUnsafe(65536);
    const rightBuffer = Buffer.allocUnsafe(65536);
    let leftSize = 0;
    let rightSize = 0;
    let firstDifference = null;
    while (true) {
      const [l, r] = await Promise.all([
        readBlock(left, leftBuffer),
        readBlock(right, rightBuffer),
      ]);
      if (!l.bytesRead && !r.bytesRead) break;
      leftHash.update(leftBuffer.subarray(0, l.bytesRead));
      rightHash.update(rightBuffer.subarray(0, r.bytesRead));
      if (firstDifference === null) {
        const common = Math.min(l.bytesRead, r.bytesRead);
        for (let i = 0; i < common; i++) {
          if (leftBuffer[i] !== rightBuffer[i]) {
            firstDifference = {
              offset: leftSize + i,
              leftByte: leftBuffer[i],
              rightByte: rightBuffer[i],
            };
            break;
          }
        }
        if (firstDifference === null && l.bytesRead !== r.bytesRead) {
          firstDifference = {
            offset: Math.min(leftSize + l.bytesRead, rightSize + r.bytesRead),
            leftByte: common < l.bytesRead ? leftBuffer[common] : null,
            rightByte: common < r.bytesRead ? rightBuffer[common] : null,
          };
        }
      }
      leftSize += l.bytesRead;
      rightSize += r.bytesRead;
    }
    const leftDigest = leftHash.digest('hex');
    const rightDigest = rightHash.digest('hex');
    const identical = leftSize === rightSize && leftDigest === rightDigest;
    console.log(JSON.stringify({
      criterion: 'complete-file-sha256-identity',
      left: { path: leftPath, bytes: leftSize, sha256: leftDigest },
      right: { path: rightPath, bytes: rightSize, sha256: rightDigest },
      identical,
      firstDifference,
      metadataNormalized: false,
      audioDecoded: false,
    }, null, 2));
    process.exitCode = identical ? 0 : 1;
  } finally {
    await Promise.all(handles.map(handle => handle.close()));
  }
}
