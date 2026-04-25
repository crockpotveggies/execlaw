import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { ScreenTransition } from "../anim/ScreenTransition";

describe("ScreenTransition", () => {
    it("renders its children", () => {
        render(
            <ScreenTransition>
                <p data-testid="child">hi</p>
            </ScreenTransition>,
        );
        expect(screen.getByTestId("child")).toHaveTextContent("hi");
    });

    it("does not throw with a non-zero delay", () => {
        expect(() =>
            render(
                <ScreenTransition delayMs={50}>
                    <span data-testid="child">delayed</span>
                </ScreenTransition>,
            ),
        ).not.toThrow();
    });
});
