import { describe, it, expect } from 'vitest';
import {
  NAME_MAX, canRestoreDefault, nameDirty, nameEditable, normalizeLocalName, parseNameSource,
} from './identityName';

describe('normalising the machine name', () => {
  it('keeps an ordinary name unchanged', () => {
    expect(normalizeLocalName('客厅 Mac')).toBe('客厅 Mac');
  });

  it('trims surrounding whitespace', () => {
    expect(normalizeLocalName('  studio-mac \t')).toBe('studio-mac');
  });

  it('reads blank input as "clear the override"', () => {
    // Not "set the name to empty": an empty override means follow the host
    // name, which is what the 恢复默认 button sends too.
    expect(normalizeLocalName('')).toBe('');
    expect(normalizeLocalName('   ')).toBe('');
    expect(normalizeLocalName('\n\t ')).toBe('');
  });

  it('deletes newlines and control characters rather than substituting them', () => {
    // This string ends up as the device name in every peer's system sound
    // settings. A pasted newline there produces a broken-looking entry that the
    // user cannot see in the input box they typed it into.
    expect(normalizeLocalName('living\nroom')).toBe('livingroom');
    expect(normalizeLocalName('a\u0000b\u0007c\u007f')).toBe('abc');
    expect(normalizeLocalName('tab\there')).toBe('tabhere');
  });

  it('truncates rather than rejecting an over-long name', () => {
    const long = 'x'.repeat(NAME_MAX + 20);
    expect(normalizeLocalName(long)).toHaveLength(NAME_MAX);
  });

  it('trims before measuring, so trailing spaces do not eat the budget', () => {
    const padded = `${'y'.repeat(NAME_MAX)}     `;
    expect(normalizeLocalName(padded)).toBe('y'.repeat(NAME_MAX));
  });
});

describe('where the name came from', () => {
  it('recognises the two sources that are not the host name', () => {
    expect(parseNameSource('env')).toBe('env');
    expect(parseNameSource('custom')).toBe('custom');
  });

  it('falls back to hostname for anything it does not recognise', () => {
    // An older daemon sends no `name_source` at all, and "hostname" is the
    // truthful reading there — it had no override to report.
    expect(parseNameSource(undefined)).toBe('hostname');
    expect(parseNameSource(null)).toBe('hostname');
    expect(parseNameSource('nonsense')).toBe('hostname');
    expect(parseNameSource(7)).toBe('hostname');
  });
});

describe('what the name row lets the user do', () => {
  it('locks the field while AUDIOHUB_NAME is in force', () => {
    // The env var outranks a stored override on purpose (regress tells several
    // daemons on one host apart with it). An editable box over a value that
    // cannot change just looks like saving failed.
    expect(nameEditable('env')).toBe(false);
    expect(nameEditable('custom')).toBe(true);
    expect(nameEditable('hostname')).toBe(true);
  });

  it('offers 恢复默认 only when an override actually exists', () => {
    expect(canRestoreDefault('custom')).toBe(true);
    expect(canRestoreDefault('hostname')).toBe(false);
    expect(canRestoreDefault('env')).toBe(false);
  });

  it('does not arm 保存 for a draft that would change nothing', () => {
    expect(nameDirty('studio', 'studio')).toBe(false);
    expect(nameDirty('  studio  ', 'studio')).toBe(false);
    expect(nameDirty('studio\n', 'studio')).toBe(false);
  });

  it('arms 保存 for a real change, including clearing the field', () => {
    expect(nameDirty('studio-2', 'studio')).toBe(true);
    expect(nameDirty('', 'studio')).toBe(true);
  });
});
