import test from 'node:test';
import assert from 'node:assert/strict';
import { verifyLinuxAbi } from '../desktop/scripts/verify-linux-abi.mjs';

const readelf = (machine, versions = 'Name: GLIBC_2.28', dynamic = '') => args =>
  args[0] === '-h' ? `Machine: ${machine}` : args[0] === '-d' ? dynamic : versions;
for (const [arch, machine] of [['x64', 'Advanced Micro Devices X86-64'], ['arm64', 'AArch64']]) {
  test(`Linux ${arch} requires matching native helpers and preserves ABI limits`, () => {
    assert.doesNotThrow(() => verifyLinuxAbi('/fixture', arch, readelf(machine)));
    assert.throws(() => verifyLinuxAbi('/fixture', arch, readelf(arch === 'x64' ? 'AArch64' : 'Advanced Micro Devices X86-64')));
    assert.throws(() => verifyLinuxAbi('/fixture', arch, readelf(machine, 'Name: GLIBC_2.29')), /requires/);
    assert.throws(() => verifyLinuxAbi('/fixture', arch, readelf(machine, '', 'Shared library: [libssl.so.3]')), /OpenSSL statically/);
  });
}
test('Linux helper checks reject unsupported architectures before probing', () => {
  assert.throws(() => verifyLinuxAbi('/fixture', 'ia32', () => { throw new Error('Probe should not run'); }), /Unsupported Linux/);
});
