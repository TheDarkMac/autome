import { describe, expect, it } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { Palette } from "./Palette";
import { DND_MIME, STATIC_CATALOG } from "./catalog";

describe("<Palette />", () => {
  it("renders each category from the static catalog", () => {
    render(<Palette items={STATIC_CATALOG} />);
    expect(screen.getByText("Triggers")).toBeInTheDocument();
    expect(screen.getByText("Actions")).toBeInTheDocument();
    expect(screen.getByText("Logique")).toBeInTheDocument();
    // Vérifie qu'on retrouve au moins quelques items emblématiques.
    expect(screen.getByText("Webhook")).toBeInTheDocument();
    expect(screen.getByText("Slack — Send Message")).toBeInTheDocument();
    expect(screen.getByText("If")).toBeInTheDocument();
  });

  it("filters items based on the search field (case-insensitive)", () => {
    render(<Palette items={STATIC_CATALOG} />);
    const search = screen.getByLabelText(/Rechercher/i);
    fireEvent.change(search, { target: { value: "slack" } });
    expect(screen.getByText("Slack — Send Message")).toBeInTheDocument();
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

  it("shows a loading message when loading=true", () => {
    render(<Palette items={[]} loading />);
    expect(screen.getByRole("status")).toHaveTextContent(/Chargement/);
  });

  it("sets the autome MIME payload on dragstart with the full item", () => {
    render(<Palette items={STATIC_CATALOG} />);
    const slack = screen.getByTestId("palette-item-action.slack.send");
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
    expect(parsed.subtype).toBe("slack.send");
  });
});
