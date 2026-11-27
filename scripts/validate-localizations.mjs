import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { resolve } from 'node:path';

export const regionKeys = ['cancel', 'confirm', 'accessibility', 'modeLabel', 'fullScreen', 'region', 'dragBar'].map(key => 'regionSelection.' + key);
export const automationHelpKeys = ['menu', 'title', 'body', 'cliTitle', 'pathLabel', 'access', 'limits', 'close',
  'command.selectionStatus', 'command.cancelSelection', 'command.targetDisplays', 'command.targetSet',
  'command.targetStatus', 'command.targetClear', 'command.captureStatus', 'command.help',
].map(key => 'automationHelp.' + key);
export function validateCatalogs(ja, en) {
  for (const [locale, catalog] of Object.entries({ja, en})) {
    if (!catalog || Array.isArray(catalog) || typeof catalog !== 'object') throw Error(`${locale}: expected a flat object`);
    for (const [key, value] of Object.entries(catalog)) {
      if (!/^[A-Za-z][A-Za-z0-9_.]*$/.test(key) || typeof value !== 'string' || !value.trim()) throw Error(`${locale}: invalid translation ${key}`);
    }
  }
  const keys = Object.keys(ja).sort();
  if (JSON.stringify(keys) !== JSON.stringify(Object.keys(en).sort())) throw Error('Japanese and English translation keys differ');
  const placeholders = text => [...text.matchAll(/\{(\w+)\}/g)].map(match => match[1]).sort();
  for (const key of keys) {
    if (JSON.stringify(placeholders(ja[key])) !== JSON.stringify(placeholders(en[key]))) throw Error(`Translation placeholders differ: ${key}`);
  }
  if (JSON.stringify(keys.filter(key => key.startsWith('regionSelection.')).sort()) !== JSON.stringify([...regionKeys].sort())) throw Error('Region selection translation keys differ');
  if (JSON.stringify(keys.filter(key => key.startsWith('automationHelp.')).sort()) !== JSON.stringify([...automationHelpKeys].sort())) throw Error('Automation Help translation keys differ');
  return {keys: keys.length, regionKeys: regionKeys.length};
}
export function validateLocalizations(root = fileURLToPath(new URL('..', import.meta.url))) {
  return validateCatalogs(...['ja', 'en'].map(locale => JSON.parse(readFileSync(resolve(root, 'src/locales', `${locale}.json`), 'utf8'))));
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) console.log(JSON.stringify(validateLocalizations()));
