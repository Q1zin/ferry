export function formatBytes(bytes: number, lowerBound = false): string {
  const units = ['Б', 'КиБ', 'МиБ', 'ГиБ', 'ТиБ']
  let unit = 0
  while (bytes >= 1024 && unit < units.length - 1) {
    bytes /= 1024
    unit++
  }
  if (lowerBound && unit) bytes = Math.floor(bytes * 10) / 10
  return (lowerBound ? 'не менее ' : '') + bytes.toLocaleString(undefined, { maximumFractionDigits: unit ? 1 : 0 }) + ' ' + units[unit]
}
