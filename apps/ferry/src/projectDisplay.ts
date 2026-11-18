export function formatBytes(bytes: number): string {
  const units = ['Б', 'КиБ', 'МиБ', 'ГиБ', 'ТиБ']
  let unit = 0
  while (bytes >= 1024 && unit < units.length - 1) {
    bytes /= 1024
    unit++
  }
  return bytes.toLocaleString(undefined, { maximumFractionDigits: unit ? 1 : 0 }) + ' ' + units[unit]
}
