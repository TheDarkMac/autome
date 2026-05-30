// Palette de nodes — sidebar à gauche du canvas (F8.3.a/b).
//
// Le composant est purement présentationnel : il consomme une liste
// d'`CatalogItem` (fournie par F8.3.c via `fetchCatalog`), groupe par
// catégorie, expose un champ de recherche, et émet le payload d'un drag
// via `dataTransfer.setData(DND_MIME, ...)`. La cible du drop est gérée
// par `NodeCanvas`.

import { useDeferredValue, useMemo, useState } from "react";
import {
  DND_MIME,
  type CatalogItem,
  groupByCategory,
  searchCatalog,
} from "./catalog";

export interface PaletteProps {
  items: readonly CatalogItem[];
  /** État de chargement — affiche un message au lieu de la liste vide. */
  loading?: boolean;
}

export function Palette({ items, loading = false }: PaletteProps): JSX.Element {
  const [query, setQuery] = useState("");
  const deferredQuery = useDeferredValue(query);

  const groups = useMemo(() => {
    const filtered = searchCatalog(items, deferredQuery);
    return groupByCategory(filtered);
  }, [items, deferredQuery]);

  return (
    <aside
      aria-label="Palette de nodes"
      className="flex h-full w-72 shrink-0 flex-col border-r border-neutral-200 bg-white"
      data-testid="palette"
    >
      <div className="border-b border-neutral-200 px-4 py-3">
        <h2 className="text-sm font-semibold text-neutral-800">Palette</h2>
        <p className="mt-0.5 text-xs text-neutral-500">
          Glissez un item vers le canvas.
        </p>
        <label htmlFor="palette-search" className="sr-only">
          Rechercher un node
        </label>
        <input
          id="palette-search"
          type="search"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="Rechercher…"
          className="mt-3 w-full rounded-md border border-neutral-200 bg-neutral-50 px-3 py-1.5 text-sm outline-none transition focus:border-sky-400 focus:bg-white"
        />
      </div>

      <div className="flex-1 overflow-y-auto px-2 py-3" role="list">
        {loading ? (
          <p className="px-2 text-sm text-neutral-500" role="status">
            Chargement du catalogue…
          </p>
        ) : groups.length === 0 ? (
          <p className="px-2 text-sm text-neutral-500" role="status">
            Aucun node ne correspond à « {query} ».
          </p>
        ) : (
          groups.map((group) => (
            <section
              key={group.category}
              aria-labelledby={`palette-cat-${group.category}`}
              className="mb-3"
            >
              <h3
                id={`palette-cat-${group.category}`}
                className="px-2 pb-1 text-xs font-medium uppercase tracking-wide text-neutral-500"
              >
                {group.category}
              </h3>
              <ul className="space-y-1">
                {group.items.map((item) => (
                  <PaletteItemView key={item.id} item={item} />
                ))}
              </ul>
            </section>
          ))
        )}
      </div>
    </aside>
  );
}

function PaletteItemView({ item }: { item: CatalogItem }): JSX.Element {
  const onDragStart = (e: React.DragEvent<HTMLLIElement>): void => {
    e.dataTransfer.setData(DND_MIME, JSON.stringify(item));
    e.dataTransfer.effectAllowed = "move";
  };
  return (
    <li
      role="button"
      tabIndex={0}
      draggable
      onDragStart={onDragStart}
      data-testid={`palette-item-${item.id}`}
      className="cursor-grab rounded-md border border-transparent px-2 py-1.5 text-sm hover:border-neutral-200 hover:bg-neutral-50 active:cursor-grabbing"
    >
      <div className="font-medium text-neutral-800">{item.label}</div>
      {item.description ? (
        <div className="text-xs text-neutral-500">{item.description}</div>
      ) : null}
    </li>
  );
}
