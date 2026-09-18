//! Re-emit the `Program` impl and the export-descriptor companion.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

use crate::export_desc::emit_program_export_desc;
use crate::parse::ProgramDef;

pub fn expand(def: ProgramDef) -> TokenStream2 {
    let ProgramDef { item, self_ty, name, intent, input, result } = def;
    let export_desc = emit_program_export_desc(&self_ty, &name, &intent, &input, &result);
    quote! {
        #item

        #export_desc
    }
}
