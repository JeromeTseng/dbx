import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const dialogSource = readFileSync(new URL("../EditorSettingsDialog.vue", import.meta.url), "utf8");
const editorSectionStart = dialogSource.indexOf(`activeSettingsTab === 'editor'`);
const formatterSectionStart = dialogSource.indexOf(`activeSettingsTab === 'formatter'`, editorSectionStart);
const editorSection = dialogSource.slice(editorSectionStart, formatterSectionStart);

describe("EditorSettingsDialog live preview placement", () => {
  it("renders one preview after the appearance controls and before behavior settings", () => {
    const fontFamily = editorSection.indexOf('t("settings.fontFamily")');
    const theme = editorSection.indexOf('t("settings.theme")');
    const fontSize = editorSection.indexOf('t("settings.fontSize")');
    const preview = editorSection.indexOf('ref="previewRef"');
    const executeMode = editorSection.indexOf("executeModeLabel");

    expect(editorSectionStart).toBeGreaterThanOrEqual(0);
    expect(formatterSectionStart).toBeGreaterThan(editorSectionStart);
    expect(fontFamily).toBeGreaterThanOrEqual(0);
    expect(theme).toBeGreaterThan(fontFamily);
    expect(fontSize).toBeGreaterThan(theme);
    expect(preview).toBeGreaterThan(fontSize);
    expect(preview).toBeLessThan(executeMode);
    expect(editorSection.match(/ref="previewRef"/g)).toHaveLength(1);
  });

  it("keeps the preview lifecycle tied to the same template ref", () => {
    expect(dialogSource).toContain("watch(previewRef, async (el) => {");
    expect(dialogSource).toContain("cleanupPreviewEditor();");
  });

  it("updates preview line-number visibility from the unsaved editor draft", () => {
    expect(dialogSource).toContain("showLineNumbers: editShowLineNumbers.value");
    expect(dialogSource).toContain('import { buildQueryEditorLineNumbersExtension } from "@/lib/editor/queryEditorLineNumbers";');
    expect(dialogSource).toContain('let previewLineNumbersComp: import("@codemirror/state").Compartment | null = null;');
    expect(dialogSource).toContain('const previewBasicSetup = (basicSetup as readonly import("@codemirror/state").Extension[]).slice(2);');
    expect(dialogSource).toContain("previewLineNumbersComp.of(buildPreviewLineNumbersExtension(ss.showLineNumbers))");
    expect(dialogSource).toContain("previewLineNumbersComp.reconfigure(buildPreviewLineNumbersExtension(ss.showLineNumbers))");
    expect(dialogSource).toContain("return buildQueryEditorLineNumbersExtension(previewLineNumbersFactory, enabled");
  });
});
