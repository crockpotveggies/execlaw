import { describe, expect, it } from "vitest";
import { clearTokens, loadTokens, saveTokens } from "../auth/tokens";

describe("token store", () => {
    it("returns null when nothing is stored", () => {
        expect(loadTokens()).toBeNull();
    });

    it("save → load round-trip", () => {
        saveTokens({ access_token: "a", refresh_token: "r" });
        expect(loadTokens()).toEqual({ access_token: "a", refresh_token: "r" });
    });

    it("returns null when only one half is stored", () => {
        // Simulate a tampered/half-cleared localStorage.
        localStorage.setItem("execlaw.access_token", "a");
        expect(loadTokens()).toBeNull();
    });

    it("clearTokens removes both halves", () => {
        saveTokens({ access_token: "a", refresh_token: "r" });
        clearTokens();
        expect(loadTokens()).toBeNull();
    });
});
