import { describe, expect, it } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { Palette } from "./Palette";
import { BUILTIN_CATALOG, DND_MIME } from "./catalog";
import { SAMPLE_CATALOG } from "./testCatalog";

const STATIC_CATALOG = SAMPLE_CATALOG.items;

describe("<Palette />", () => {
  it("renders each category from the loaded catalog", () => {
    render(<Palette items={STATIC_CATALOG} />);
    expect(screen.getByText("Triggers")).toBeInTheDocument();
    expect(screen.getByText("Actions")).toBeInTheDocument();
    expect(screen.getByText("Logique")).toBeInTheDocument();
    // Vérifie qu'on retrouve au moins quelques items emblématiques.
    expect(screen.getByText("Webhook")).toBeInTheDocument();
    expect(screen.getByText("Slack — Envoyer un message")).toBeInTheDocument();
    expect(screen.getByText("If")).toBeInTheDocument();
  });

  it("filters items based on the search field (case-insensitive)", () => {
    render(<Palette items={STATIC_CATALOG} />);
    const search = screen.getByLabelText(/Rechercher/i);
    fireEvent.change(search, { target: { value: "slack" } });
    expect(screen.getByText("Slack — Envoyer un message")).toBeInTheDocument();
    expect(screen.queryByText("Webhook")).not.toBeInTheDocument();
    expect(screen.queryByText("If")).not.toBeInTheDocument();
  });

  it("shows a no-result message when nothing matches", () => {
    render(<Palette items={STATIC_CATALOG} />);
    fireEvent.change(screen.getByLabelText(/Rechercher/i), {
      target: { value: "zzz-not-a-node" },
    });
    expect(screen.getByRole("status")).toHaveTextContent(/Aucun node/);
  });

  it("signale des actions de connecteur indisponibles (#257)", () => {
    const { rerender } = render(<Palette items={BUILTIN_CATALOG} connectorsUnavailable />);
    expect(screen.getByTestId("palette-connectors-unavailable")).toHaveTextContent(
      /Actions de connecteur indisponibles/,
    );
    expect(screen.getByText("HTTP Request")).toBeInTheDocument();
    rerender(<Palette items={BUILTIN_CATALOG} connectorsUnavailable loading />);
    expect(screen.queryByTestId("palette-connectors-unavailable")).toBeNull();
  });

  it("shows a loading message when loading=true", () => {
    render(<Palette items={[]} loading />);
    expect(screen.getByRole("status")).toHaveTextContent(/Chargement/);
  });

  it("sets the autome MIME payload on dragstart with the full item", () => {
    render(<Palette items={STATIC_CATALOG} />);
    const slack = screen.getByTestId("palette-item-action.slack.send_message");
    let captured: string | undefined;
    const setData = (type: string, value: string): void => {
      if (type === DND_MIME) captured = value;
    };
    const dataTransfer = {
      setData,
      getData: () => "",
      dropEffect: "none",
      effectAllowed: "none",
    } as unknown as DataTransfer;
    fireEvent.dragStart(slack, { dataTransfer });
    expect(captured).toBeDefined();
    const parsed = JSON.parse(captured!) as { kind: string; subtype?: string };
    expect(parsed.kind).toBe("action");
    expect(parsed.subtype).toBe("slack.send_message");
  });
});
