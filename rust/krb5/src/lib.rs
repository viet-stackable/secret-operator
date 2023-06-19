//! Safe wrapper library for libkrb5 and libkadm5
//!
//! The primary entry point is [`KrbContext`].

use std::{
    ffi::{c_char, c_int, CStr},
    fmt::{Debug, Display},
    ops::Deref,
};

use krb5_sys::krb5_kt_resolve;
use profile::Profile;
use snafu::{ResultExt, Snafu};

pub mod kadm5;
pub mod profile;

/// An error generated by libkrb5, or from interacting with it
#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("{reason}"))]
    Krb5 { reason: Krb5Error },
    #[snafu(display("{string_name} is too long"))]
    StringTooLong {
        source: std::num::TryFromIntError,
        string_name: &'static str,
    },
}
/// An error generated by libkrb5
#[derive(Debug)]
pub struct Krb5Error {
    message: String,
    pub code: krb5_sys::krb5_error_code,
}
impl Error {
    // SAFETY: must be called exactly once, immediately after each potentially
    // error-generating call that interacts with ctx
    // ctx should be None iff the error happened during ctx init
    unsafe fn from_call_result(
        ctx: Option<&KrbContext>,
        code: krb5_sys::krb5_error_code,
    ) -> Result<(), Self> {
        if code.0 == 0 {
            Ok(())
        } else {
            let message = {
                // copy message into rust str, to avoid keeping a dependency on ctx
                // also, krb5_get_error_message may only be called once per error
                let raw_ctx = ctx.map_or(std::ptr::null_mut(), |c| c.raw);
                let c_msg = unsafe { krb5_sys::krb5_get_error_message(raw_ctx, code) };
                let rust_msg = CStr::from_ptr(c_msg).to_string_lossy().into_owned();
                unsafe { krb5_sys::krb5_free_error_message(raw_ctx, c_msg) }
                rust_msg
            };
            Krb5Snafu {
                reason: Krb5Error { message, code },
            }
            .fail()
        }
    }
}
impl Display for Krb5Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result where {
        f.write_str(&self.message)
    }
}

/// An instance of the krb5 client
///
/// Most other `krb5` data structures are linked to a specific `KrbContext`,
/// and must not be mixed between them or used past the lifetime of the owning `KrbContext`.
/// In Rust-land we represent this as them taking a borrow on their `KrbContext`.
///
/// `KrbContext` is _not_ thread-safe, since it is mutated internally by libkrb5.
pub struct KrbContext {
    raw: krb5_sys::krb5_context,
}
impl KrbContext {
    /// Create a new context using the default configuration sources.
    pub fn new() -> Result<Self, Error> {
        let mut ctx = std::ptr::null_mut();
        unsafe { Error::from_call_result(None, krb5_sys::krb5_init_context(&mut ctx)) }?;
        Ok(Self { raw: ctx })
    }

    /// Create a new context from a given [`Profile`].
    /// `profile` will be copied into the created `Context`.
    pub fn from_profile(profile: &Profile) -> Result<Self, Error> {
        let mut ctx = std::ptr::null_mut();
        unsafe {
            Error::from_call_result(
                None,
                krb5_sys::krb5_init_context_profile(profile.raw, 0, &mut ctx),
            )
        }?;
        Ok(Self { raw: ctx })
    }

    /// Parse a Kerberos principal into a [`Principal`].
    ///
    /// This will be done in the scope of the context, for example the context's default realm will be used if
    /// none is specified in `princ_name`.
    pub fn parse_principal_name(&self, princ_name: &CStr) -> Result<Principal, Error> {
        let mut principal = std::ptr::null_mut();
        unsafe {
            Error::from_call_result(
                None,
                krb5_sys::krb5_parse_name(self.raw, princ_name.as_ptr(), &mut principal),
            )
        }?;
        Ok(Principal {
            ctx: self,
            raw: principal,
        })
    }

    /// Get the default realm configured for this context.
    pub fn default_realm(&self) -> Result<DefaultRealm, Error> {
        let mut realm: *mut c_char = std::ptr::null_mut();
        unsafe {
            Error::from_call_result(
                Some(self),
                krb5_sys::krb5_get_default_realm(self.raw, &mut realm),
            )?;
            Ok(DefaultRealm {
                ctx: self,
                raw: realm,
            })
        }
    }
}
impl Drop for KrbContext {
    fn drop(&mut self) {
        unsafe {
            krb5_sys::krb5_free_context(self.raw);
        }
    }
}

/// The default realm name for a [`KrbContext`].
///
/// Created by [`KrbContext::default_realm`].
pub struct DefaultRealm<'a> {
    ctx: &'a KrbContext,
    raw: *const c_char,
}
impl Deref for DefaultRealm<'_> {
    type Target = CStr;

    fn deref(&self) -> &Self::Target {
        unsafe { CStr::from_ptr(self.raw) }
    }
}
impl Drop for DefaultRealm<'_> {
    fn drop(&mut self) {
        unsafe { krb5_sys::krb5_free_default_realm(self.ctx.raw, self.raw.cast_mut()) }
    }
}

/// A parsed Kerberos principal name.
///
/// Created by [`KrbContext::parse_principal_name`].
pub struct Principal<'a> {
    ctx: &'a KrbContext,
    raw: krb5_sys::krb5_principal,
}
impl<'a> Principal<'a> {
    /// The default salt when deriving keys for this principal.
    pub fn default_salt(&self) -> Result<KrbData<'a>, Error> {
        unsafe {
            let mut salt = std::mem::zeroed::<krb5_sys::krb5_data>();
            Error::from_call_result(
                Some(self.ctx),
                krb5_sys::krb5_principal2salt(self.ctx.raw, self.raw, &mut salt),
            )?;
            Ok(KrbData {
                ctx: self.ctx,
                raw: salt,
            })
        }
    }

    /// Converts the parsed principal back into a string representation.
    ///
    /// The [`Display`] instance is equivalent to `self.unparse(PrincipalUnparseOptions::default())`.
    pub fn unparse(&self, options: PrincipalUnparseOptions) -> Result<String, Error> {
        let mut raw_name = std::ptr::null_mut();
        unsafe {
            Error::from_call_result(
                Some(self.ctx),
                krb5_sys::krb5_unparse_name_flags(
                    self.ctx.raw,
                    self.raw,
                    options.to_flags(),
                    &mut raw_name,
                ),
            )?;
        };
        // We need to take ownership before freeing it
        let name: String = unsafe { CStr::from_ptr(raw_name) }
            .to_string_lossy()
            .into_owned();
        unsafe { krb5_sys::krb5_free_unparsed_name(self.ctx.raw, raw_name) }
        Ok(name)
    }
}
impl Drop for Principal<'_> {
    fn drop(&mut self) {
        unsafe {
            krb5_sys::krb5_free_principal(self.ctx.raw, self.raw);
        }
    }
}
impl Display for Principal<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = self.unparse(PrincipalUnparseOptions::default());
        f.write_str(name.as_deref().unwrap_or("(invalid)"))
    }
}
impl From<&Principal<'_>> for String {
    fn from(princ: &Principal<'_>) -> Self {
        princ.to_string()
    }
}

/// Optional settings for [`Principal::unparse`].
#[derive(Default, Clone, Copy)]
pub struct PrincipalUnparseOptions {
    /// Controls whether the realm is included.
    pub realm: PrincipalRealmDisplayMode,
    /// Special characters are not quoted in display mode, even if this would generate a principal string that cannot be parsed.
    pub for_display: bool,
}

/// See [`PrincipalUnparseOptions::realm`].
#[derive(Default, Clone, Copy)]
pub enum PrincipalRealmDisplayMode {
    /// The realm is always included.
    #[default]
    Always,
    /// The realm is only included if it is not the default realm.
    IfForeign,
    /// The realm is never included. This may create ambiguity in multi-realm configurations.
    Never,
}
impl PrincipalUnparseOptions {
    fn to_flags(self) -> c_int {
        let realm = match self.realm {
            PrincipalRealmDisplayMode::Always => 0,
            PrincipalRealmDisplayMode::IfForeign => krb5_sys::KRB5_PRINCIPAL_UNPARSE_SHORT as c_int,
            PrincipalRealmDisplayMode::Never => krb5_sys::KRB5_PRINCIPAL_UNPARSE_NO_REALM as c_int,
        };
        let for_display = match self.for_display {
            true => krb5_sys::KRB5_PRINCIPAL_UNPARSE_DISPLAY as c_int,
            false => 0,
        };
        realm | for_display
    }
}

/// A reference to a Kerberos keyblock.
// SAFETY: 'a must not outlive the object that owns the `KeyblockRef`
pub struct KeyblockRef<'a> {
    // We need to constrain the lifetime to the owning KrbContext even if it is never actually used
    #[allow(dead_code)]
    ctx: &'a KrbContext,
    raw: *const krb5_sys::krb5_keyblock,
}

/// An owned reference to a Kerberos keyblock.
pub struct Keyblock<'a> {
    ctx: &'a KrbContext,
    raw: *mut krb5_sys::krb5_keyblock,
}
impl<'a> Keyblock<'a> {
    /// Create a new zero-initialized keyblock of a given size.
    pub fn new(
        ctx: &'a KrbContext,
        enctype: krb5_sys::krb5_enctype,
        len: u64,
    ) -> Result<Self, Error> {
        unsafe {
            let mut keyblock: *mut krb5_sys::krb5_keyblock = std::ptr::null_mut();
            Error::from_call_result(
                Some(ctx),
                krb5_sys::krb5_init_keyblock(ctx.raw, enctype, len, &mut keyblock),
            )?;
            let mut kb = Self { ctx, raw: keyblock };
            // krb5_init_keyblock does not guarantee that the keyblock is zeroed, so let's clear it ourselves to avoid leaks
            kb.contents_mut()?.fill(0);
            Ok(kb)
        }
    }

    /// Derive a key from a given password.
    ///
    /// Some well-known `enctype` values are available in [`enctype`].
    ///
    /// `salt` may be generated using [`Principal::default_salt`].
    pub fn from_password(
        ctx: &'a KrbContext,
        enctype: krb5_sys::krb5_enctype,
        password: &CStr,
        salt: &KrbData,
    ) -> Result<Self, Error> {
        let kb = Self::new(
            ctx, enctype,
            // not that we have a reason to use a preinitialized keyblock,
            // but `krb5_c_string_to_key` doesn't free or reuse an existing
            // (non-null) keyblock's contents
            0,
        )?;
        let password_data = krb5_sys::krb5_data {
            magic: krb5_sys::krb5_error_code(0),
            length: password
                .to_bytes()
                .len()
                .try_into()
                .context(StringTooLongSnafu {
                    string_name: "password",
                })?,
            data: password.as_ptr().cast::<c_char>().cast_mut(),
        };
        unsafe {
            Error::from_call_result(
                Some(ctx),
                krb5_sys::krb5_c_string_to_key(ctx.raw, enctype, &password_data, &salt.raw, kb.raw),
            )?;
        }
        Ok(kb)
    }

    // SAFETY: we own raw, so it is valid for as long as the reference to &śelf
    pub fn contents_mut(&mut self) -> Result<&mut [u8], Error> {
        unsafe {
            let raw = *self.raw;
            if raw.length > 0 {
                Ok(std::slice::from_raw_parts_mut(
                    raw.contents,
                    raw.length.try_into().context(StringTooLongSnafu {
                        string_name: "keyblock",
                    })?,
                ))
            } else {
                // contents are not allocated for length=0, but slice requires that the ptr is non-null and "valid"
                Ok(&mut [])
            }
        }
    }

    // Ideally this would be a Deref impl, but we don't have a KeyblockRef we can borrow
    // SAFETY: the KeyblockRef must not outlive the &self-ref
    #[allow(clippy::needless_lifetimes)]
    pub fn as_ref<'b>(&'b self) -> KeyblockRef<'b> {
        KeyblockRef {
            ctx: self.ctx,
            raw: self.raw,
        }
    }
}
impl<'a> Drop for Keyblock<'a> {
    fn drop(&mut self) {
        unsafe {
            krb5_sys::krb5_free_keyblock(self.ctx.raw, self.raw);
        }
    }
}

/// Well-known encryption types. This is not exhaustive.
pub mod enctype {
    pub const AES256_CTS_HMAC_SHA1_96: krb5_sys::krb5_enctype =
        krb5_sys::ENCTYPE_AES256_CTS_HMAC_SHA1_96 as i32;
}

/// A Kerberos keytab.
pub struct Keytab<'a> {
    ctx: &'a KrbContext,
    raw: krb5_sys::krb5_keytab,
}
impl<'a> Keytab<'a> {
    /// Create a `Keytab` for a given name.
    ///
    /// `name` should follow the format `{type}:{residual}`, such as `FILE:/foo/bar`.
    /// Known types are:
    /// - `FILE`: A keytab serialized to a file.
    /// - `MEMORY`: An in-memory keytab.
    ///
    /// The file, if used, does not need to exist. It will be created as required.
    pub fn resolve(ctx: &'a KrbContext, name: &CStr) -> Result<Self, Error> {
        let mut raw = std::ptr::null_mut();
        unsafe {
            Error::from_call_result(Some(ctx), krb5_kt_resolve(ctx.raw, name.as_ptr(), &mut raw))?
        }
        Ok(Self { ctx, raw })
    }

    /// Add the specified key to the keytab.
    pub fn add(
        &mut self,
        principal: &Principal,
        kvno: krb5_sys::krb5_kvno,
        keyblock: &KeyblockRef,
    ) -> Result<(), Error> {
        unsafe {
            let mut entry: krb5_sys::krb5_keytab_entry = std::mem::zeroed();
            entry.principal = principal.raw;
            entry.vno = kvno;
            entry.key = keyblock.raw.read();
            // SAFETY: krb5_kt_add_entry is responsible for copying entry as needed
            Error::from_call_result(
                Some(self.ctx),
                krb5_sys::krb5_kt_add_entry(self.ctx.raw, self.raw, &mut entry),
            )
        }
    }
}
impl Drop for Keytab<'_> {
    fn drop(&mut self) {
        unsafe {
            Error::from_call_result(
                Some(self.ctx),
                krb5_sys::krb5_kt_close(self.ctx.raw, self.raw),
            )
            .unwrap()
        }
    }
}

/// Opaque Kerberos data
pub struct KrbData<'a> {
    ctx: &'a KrbContext,
    raw: krb5_sys::krb5_data,
}
impl Debug for KrbData<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let slice = unsafe {
            std::slice::from_raw_parts(
                self.raw.data.cast::<u8>(),
                self.raw.length.try_into().unwrap(),
            )
        };
        let s = std::str::from_utf8(slice).unwrap();
        Debug::fmt(s, f)
    }
}
impl Drop for KrbData<'_> {
    fn drop(&mut self) {
        unsafe { krb5_sys::krb5_free_data_contents(self.ctx.raw, &mut self.raw) }
    }
}
