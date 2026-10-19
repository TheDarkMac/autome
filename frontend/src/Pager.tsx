// Pagination des listes de l'administration (comptes #208, workflows #243).

export function Pager({
  offset,
  count,
  total,
  pageSize,
  onChange,
  label = "Pagination",
}: {
  offset: number;
  count: number;
  total: number;
  pageSize: number;
  onChange: (offset: number) => void;
  /** Nom accessible : distingue les paginations d'une même page. */
  label?: string;
}): JSX.Element {
  return (
    <nav aria-label={label} className="mt-3 flex items-center gap-3 text-sm">
      <span className="text-neutral-500">
        {offset + 1}–{offset + count} sur {total}
      </span>
      <button
        type="button"
        disabled={offset === 0}
        onClick={() => onChange(Math.max(0, offset - pageSize))}
        className="rounded-md border border-neutral-200 px-2 py-1 text-neutral-700 hover:bg-neutral-50 disabled:opacity-60"
      >
        Précédent
      </button>
      <button
        type="button"
        disabled={offset + count >= total}
        onClick={() => onChange(offset + pageSize)}
        className="rounded-md border border-neutral-200 px-2 py-1 text-neutral-700 hover:bg-neutral-50 disabled:opacity-60"
      >
        Suivant
      </button>
    </nav>
  );
}
