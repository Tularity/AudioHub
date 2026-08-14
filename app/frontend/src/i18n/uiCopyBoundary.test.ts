import { readFileSync, readdirSync } from 'node:fs';
import { join, relative } from 'node:path';
import ts from 'typescript';
import { describe, expect, it } from 'vitest';

const srcRoot = join(process.cwd(), 'src');

function tsxFiles(dir = srcRoot): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) return tsxFiles(path);
    return entry.name.endsWith('.tsx') && !entry.name.endsWith('.test.tsx') ? [path] : [];
  });
}

/**
 * 这里仅拦最确定的两种漏翻：裸 JSX 文本与可读属性里的字符串。className、协议
 * token、data-testid 等不能靠“看见字符串就禁”来判断，否则守卫只会变成误报清单。
 */
describe('React copy boundary', () => {
  it('keeps visible JSX copy behind the i18n catalogue', () => {
    const found: string[] = [];
    const allowed = new Set(['components/ShortcutSheet.tsx:Esc']);

    for (const file of tsxFiles()) {
      const source = readFileSync(file, 'utf8');
      const ast = ts.createSourceFile(file, source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
      const rel = relative(srcRoot, file);

      const visit = (node: ts.Node): void => {
        if (ts.isJsxText(node)) {
          const text = node.getText(ast).trim();
          if (text && !allowed.has(`${rel}:${text}`)) found.push(`${rel}: JSX ${JSON.stringify(text)}`);
        }
        if (ts.isJsxAttribute(node)
          && ['aria-label', 'title', 'placeholder', 'alt'].includes(node.name.getText(ast))
          && node.initializer
          && ts.isStringLiteral(node.initializer)
          && node.initializer.text) {
          found.push(`${rel}: ${node.name.getText(ast)}=${JSON.stringify(node.initializer.text)}`);
        }
        ts.forEachChild(node, visit);
      };
      visit(ast);
    }

    expect(found).toEqual([]);
  });

  it('does not render native daemon prose in the connection overlay', () => {
    const source = readFileSync(join(srcRoot, 'components', 'Chrome.tsx'), 'utf8');
    const body = source.slice(source.indexOf('function overlayCopy'), source.indexOf('export function Overlay'));
    expect(body).not.toContain('err.message');
    expect(body).not.toContain('err.detail');
  });
});
