//! Generates a "complete" config struct and its "partial" (all-fields-optional)
//! counterpart from a single field list, so a new field only needs to be added
//! in one place instead of separately to the struct, the partial struct, the
//! merge logic, and the upgrade logic.
//!
//! Field kinds:
//! - `required pub name: Type = "error if missing",` — must be present to upgrade.
//! - `default pub name: Type = default_expr,` — falls back to a default value.
//! - `default pub name: Option<Type> = None,` — optional field that defaults to None.
//! - `nested pub name: Complete as Partial = "error if missing",` — delegates to
//!   the nested type's own `merge_other`/`upgrade_to_complete`.
//! - `partial_only pub name: Type,` — exists only on the partial struct (e.g. a
//!   key used during merging that never makes it into the complete config).
//!
//! On conflicts during merge, `required`/`default`/`partial_only` fields keep
//! whichever side already has a value (`self` wins over `other`); `nested`
//! fields recurse into the nested type's merge.
//!
//! `self`/`other` are threaded through the recursive munch as captured `ident`
//! metavariables (`$self_tok`/`$other_tok`) rather than being re-typed as bare
//! `self`/`other` tokens in each rule — macro hygiene otherwise treats each
//! fresh occurrence as a distinct binding and the generated code fails to
//! resolve them.
macro_rules! partial_config {
    (
        $(#[$smeta:meta])*
        pub struct $complete:ident / $partial:ident {
            $($body:tt)*
        }
    ) => {
        partial_config! {
            @entry
            self
            other
            $(#[$smeta])*
            pub struct $complete / $partial { $($body)* }
        }
    };

    (
        @entry
        $self_tok:ident
        $other_tok:ident
        $(#[$smeta:meta])*
        pub struct $complete:ident / $partial:ident {
            $($body:tt)*
        }
    ) => {
        partial_config! {
            @munch
            self_tok = $self_tok,
            other_tok = $other_tok,
            complete_name = $complete,
            partial_name = $partial,
            complete_meta = [$(#[$smeta])*],
            complete_fields = [],
            partial_fields = [],
            merge_arms = [],
            upgrade_arms = [],
            rest = [ $($body)* ],
        }
    };

    (
        @munch
        self_tok = $self_tok:ident,
        other_tok = $other_tok:ident,
        complete_name = $complete:ident,
        partial_name = $partial:ident,
        complete_meta = [$($cmeta:tt)*],
        complete_fields = [$($cf:tt)*],
        partial_fields = [$($pf:tt)*],
        merge_arms = [$($ma:tt)*],
        upgrade_arms = [$($ua:tt)*],
        rest = [ $(#[$fmeta:meta])* required pub $field:ident : $ty:ty = $msg:literal , $($rest:tt)* ],
    ) => {
        partial_config! {
            @munch
            self_tok = $self_tok,
            other_tok = $other_tok,
            complete_name = $complete,
            partial_name = $partial,
            complete_meta = [$($cmeta)*],
            complete_fields = [$($cf)* $(#[$fmeta])* pub $field: $ty,],
            partial_fields = [$($pf)* $(#[$fmeta])* pub $field: Option<$ty>,],
            merge_arms = [$($ma)* $field: $self_tok.$field.clone().or($other_tok.$field.clone()),],
            upgrade_arms = [$($ua)* $field: $self_tok.$field.clone().ok_or_else(|| anyhow::anyhow!($msg))?,],
            rest = [ $($rest)* ],
        }
    };

    (
        @munch
        self_tok = $self_tok:ident,
        other_tok = $other_tok:ident,
        complete_name = $complete:ident,
        partial_name = $partial:ident,
        complete_meta = [$($cmeta:tt)*],
        complete_fields = [$($cf:tt)*],
        partial_fields = [$($pf:tt)*],
        merge_arms = [$($ma:tt)*],
        upgrade_arms = [$($ua:tt)*],
        rest = [ $(#[$fmeta:meta])* default pub $field:ident : $ty:ty = $val:expr , $($rest:tt)* ],
    ) => {
        partial_config! {
            @munch
            self_tok = $self_tok,
            other_tok = $other_tok,
            complete_name = $complete,
            partial_name = $partial,
            complete_meta = [$($cmeta)*],
            complete_fields = [$($cf)* $(#[$fmeta])* pub $field: $ty,],
            partial_fields = [$($pf)* $(#[$fmeta])* pub $field: $ty,],
            merge_arms = [$($ma)* $field: $self_tok.$field.clone().or_else(|| $other_tok.$field.clone()),],
            upgrade_arms = [$($ua)* $field: $self_tok.$field.clone(),],
            rest = [ $($rest)* ],
        }
    };

    (
        @munch
        self_tok = $self_tok:ident,
        other_tok = $other_tok:ident,
        complete_name = $complete:ident,
        partial_name = $partial:ident,
        complete_meta = [$($cmeta:tt)*],
        complete_fields = [$($cf:tt)*],
        partial_fields = [$($pf:tt)*],
        merge_arms = [$($ma:tt)*],
        upgrade_arms = [$($ua:tt)*],
        rest = [ $(#[$fmeta:meta])* nested pub $field:ident : $cty:ty as $pty:ty = $msg:literal , $($rest:tt)* ],
    ) => {
        partial_config! {
            @munch
            self_tok = $self_tok,
            other_tok = $other_tok,
            complete_name = $complete,
            partial_name = $partial,
            complete_meta = [$($cmeta)*],
            complete_fields = [$($cf)* $(#[$fmeta])* pub $field: $cty,],
            partial_fields = [$($pf)* $(#[$fmeta])* pub $field: Option<$pty>,],
            merge_arms = [$($ma)* $field: match (&$self_tok.$field, &$other_tok.$field) {
                (Some(a), Some(b)) => Some(a.merge_other(b)?),
                (Some(a), None) => Some(a.clone()),
                (None, Some(b)) => Some(b.clone()),
                (None, None) => None,
            },],
            upgrade_arms = [$($ua)* $field: $self_tok.$field.as_ref().ok_or_else(|| anyhow::anyhow!($msg))?.upgrade_to_complete()?,],
            rest = [ $($rest)* ],
        }
    };

    (
        @munch
        self_tok = $self_tok:ident,
        other_tok = $other_tok:ident,
        complete_name = $complete:ident,
        partial_name = $partial:ident,
        complete_meta = [$($cmeta:tt)*],
        complete_fields = [$($cf:tt)*],
        partial_fields = [$($pf:tt)*],
        merge_arms = [$($ma:tt)*],
        upgrade_arms = [$($ua:tt)*],
        rest = [ $(#[$fmeta:meta])* partial_only pub $field:ident : $ty:ty , $($rest:tt)* ],
    ) => {
        partial_config! {
            @munch
            self_tok = $self_tok,
            other_tok = $other_tok,
            complete_name = $complete,
            partial_name = $partial,
            complete_meta = [$($cmeta)*],
            complete_fields = [$($cf)*],
            partial_fields = [$($pf)* $(#[$fmeta])* pub $field: Option<$ty>,],
            merge_arms = [$($ma)* $field: $self_tok.$field.clone().or($other_tok.$field.clone()),],
            upgrade_arms = [$($ua)*],
            rest = [ $($rest)* ],
        }
    };

    (
        @munch
        self_tok = $self_tok:ident,
        other_tok = $other_tok:ident,
        complete_name = $complete:ident,
        partial_name = $partial:ident,
        complete_meta = [$($cmeta:tt)*],
        complete_fields = [$($cf:tt)*],
        partial_fields = [$($pf:tt)*],
        merge_arms = [$($ma:tt)*],
        upgrade_arms = [$($ua:tt)*],
        rest = [],
    ) => {
        $($cmeta)*
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $complete {
            $($cf)*
        }

        #[derive(Debug, Clone, Default, PartialEq, Eq)]
        pub struct $partial {
            $($pf)*
        }

        impl $partial {
            pub fn merge_other(&$self_tok, $other_tok: &$partial) -> Result<$partial, anyhow::Error> {
                Ok($partial {
                    $($ma)*
                })
            }

            pub fn upgrade_to_complete(&$self_tok) -> Result<$complete, anyhow::Error> {
                Ok($complete {
                    $($ua)*
                })
            }
        }
    };
}

pub(crate) use partial_config;
