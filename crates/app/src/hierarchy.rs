//! Mapping from individual hideout modules to the in-game "area" they live
//! under on the Facility Upgrade screen. Shared by the state layer (to
//! cascade module-disable into the wishlist aggregation) and by the hideout
//! pane (to render parent/child rows + the disable toggle on synthetic
//! parent headers).
//!
//! Reconstructed from in-game screenshots — `data.json` keeps a flat
//! module list. See `memory/hideout_hierarchy.md` for the source-of-truth
//! mapping and rationale.

/// The category label a module renders under, or `None` if it sits at the
/// top level alongside its peers (Generator, Workshop, …).
pub fn module_category(id: &str) -> Option<&'static str> {
    match id {
        "KitchenArea" | "MicrowaveOven" | "CoffeeMaker" | "Refrigerator" => Some("Kitchen Area"),
        "MedicalArea" | "Planting" | "MedDesk" | "OperationBed" => Some("Medical Area"),
        "Storagevaluable" | "Quality" | "Moreitem" => Some("Storage Zone"),
        "StorageZoneLock1" | "StorageZoneLock2" | "StorageZoneLock3" => {
            Some("Storage Zone (A/B/C)")
        }
        "Bookcase" | "Sofa" | "TVSet" => Some("Lounge"),
        _ => None,
    }
}

/// Stable id used to store a category's disable state inside the existing
/// `disabled_modules` set. Every category — even one whose label matches a
/// buildable module's name — gets its own virtual id so that the category
/// header and the same-named module can both be toggled independently. The
/// `@cat:` prefix can't collide with real module ids (none contain `@` or
/// a colon).
pub fn category_virtual_id(category: &str) -> String {
    format!("@cat:{category}")
}

/// Id whose presence in `disabled_modules` should cascade-disable
/// `module_id` — the virtual id of the module's category, or `None` for
/// top-level uncategorized modules. Categorized modules (including those
/// whose name matches the category label, like KitchenArea inside the
/// "Kitchen Area" category) all hang under the synthetic header.
pub fn parent_disable_id(module_id: &str) -> Option<String> {
    module_category(module_id).map(category_virtual_id)
}
