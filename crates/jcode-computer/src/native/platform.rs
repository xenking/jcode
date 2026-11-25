use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFIndex, CFRange, CFType, CFTypeID, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use core_graphics::window::{copy_window_info, kCGWindowListExcludeDesktopElements};
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSRunningApplication;
use objc2_foundation::ns_string;
use std::ffi::c_void;
use std::ptr;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
    fn AXUIElementGetTypeID() -> CFTypeID;
    fn AXUIElementCopyAttributeValue(el: CFTypeRef, attr: CFStringRef, out: *mut CFTypeRef) -> i32;
    fn AXUIElementCopyAttributeValues(
        el: CFTypeRef,
        attr: CFStringRef,
        start: CFIndex,
        count: CFIndex,
        out: *mut CFArrayRef,
    ) -> i32;
    fn AXUIElementGetAttributeValueCount(
        el: CFTypeRef,
        attr: CFStringRef,
        out: *mut CFIndex,
    ) -> i32;
    fn AXUIElementCopyActionNames(el: CFTypeRef, out: *mut CFArrayRef) -> i32;
    fn AXUIElementPerformAction(el: CFTypeRef, action: CFStringRef) -> i32;
    fn AXUIElementIsAttributeSettable(el: CFTypeRef, attr: CFStringRef, settable: *mut u8) -> i32;
    fn AXUIElementSetAttributeValue(el: CFTypeRef, attr: CFStringRef, value: CFTypeRef) -> i32;
    fn AXUIElementSetMessagingTimeout(el: CFTypeRef, seconds: f32) -> i32;
    fn AXValueGetTypeID() -> CFTypeID;
    fn AXValueGetValue(value: CFTypeRef, kind: u32, out: *mut c_void) -> u8;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGRectMakeWithDictionaryRepresentation(dict: CFDictionaryRef, out: *mut CGRect) -> bool;
    fn CGPreflightScreenCaptureAccess() -> bool;
}

pub(crate) fn capture_trusted() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

pub(crate) fn trusted() -> bool {
    unsafe { AXIsProcessTrusted() != 0 }
}

pub(crate) fn probe_accessibility() -> Option<i32> {
    // Unlike the focused-app query, this doesn't depend on which app has focus.
    let pid = autoreleasepool(|_| {
        [ns_string!("com.apple.finder"), ns_string!("com.apple.dock")]
            .into_iter()
            .find_map(|id| {
                let pid = NSRunningApplication::runningApplicationsWithBundleIdentifier(id)
                    .firstObject()?
                    .processIdentifier();
                (pid > 0).then_some(pid)
            })
    })?;
    let code = match Element::application(pid).and_then(|app| app.copy_attr("AXRole")) {
        Ok(Some(_)) => 0,
        Ok(None) => -25212, // kAXErrorNoValue
        Err(err) => err.0,
    };
    Some(code)
}

#[derive(Clone, Copy, Debug)]
pub(super) struct AxError(pub(super) i32);

impl AxError {
    pub(super) fn invalid_element(self) -> bool {
        self.0 == -25202
    }

    pub(super) fn uncertain_delivery(self) -> bool {
        matches!(self.0, -25200 | -25204)
    }

    fn optional(self) -> bool {
        matches!(self.0, -25205 | -25206 | -25212)
    }
}

type Result<T> = std::result::Result<T, AxError>;

fn success(code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(AxError(code))
    }
}

#[derive(Clone, PartialEq)]
pub(super) struct Element(CFType);

// AXUIElement is a retained, opaque client-side reference. It has no AppKit
// thread affinity. A session serializes all access to its retained references.
unsafe impl Send for Element {}

impl Element {
    fn from_cf(value: CFType) -> Result<Option<Self>> {
        if value.type_of() != unsafe { AXUIElementGetTypeID() } {
            return Ok(None);
        }
        Self::timed(value).map(Some)
    }

    fn timed(value: CFType) -> Result<Self> {
        let element = Self(value);
        success(unsafe { AXUIElementSetMessagingTimeout(element.raw(), 0.25) })?;
        Ok(element)
    }

    pub(super) fn application(pid: i32) -> Result<Self> {
        let ptr = unsafe { AXUIElementCreateApplication(pid) };
        if ptr.is_null() {
            return Err(AxError(-25202));
        }
        Self::timed(unsafe { CFType::wrap_under_create_rule(ptr) })
    }

    fn raw(&self) -> CFTypeRef {
        self.0.as_CFTypeRef()
    }

    fn copy_attr(&self, attr: &str) -> Result<Option<CFType>> {
        let attr = CFString::new(attr);
        let mut out = ptr::null();
        // SAFETY: The element and attribute are live, and out is writable.
        success(unsafe {
            AXUIElementCopyAttributeValue(self.raw(), attr.as_concrete_TypeRef(), &mut out)
        })?;
        Ok((!out.is_null()).then(|| {
            // SAFETY: A successful Copy returns a retained value when non-null.
            unsafe { CFType::wrap_under_create_rule(out) }
        }))
    }

    fn attr(&self, attr: &str) -> Result<Option<CFType>> {
        match self.copy_attr(attr) {
            Err(e) if e.optional() => Ok(None),
            result => result,
        }
    }

    pub(super) fn element(&self, attr: &str) -> Result<Option<Self>> {
        self.attr(attr)?
            .map(Self::from_cf)
            .transpose()
            .map(Option::flatten)
    }

    pub(super) fn string(&self, attr: &str) -> Result<Option<String>> {
        Ok(self
            .attr(attr)?
            .and_then(|v| v.downcast_into::<CFString>())
            .map(|s| s.to_string()))
    }

    pub(super) fn value(&self) -> Result<Option<String>> {
        Ok(self.attr("AXValue")?.and_then(|v| {
            if let Some(s) = v.downcast::<CFString>() {
                Some(s.to_string())
            } else if let Some(b) = v.downcast::<CFBoolean>() {
                Some(bool::from(b).to_string())
            } else {
                v.downcast::<CFNumber>()?.to_f64().map(|n| n.to_string())
            }
        }))
    }

    pub(super) fn boolean(&self, attr: &str) -> Result<Option<bool>> {
        Ok(self
            .attr(attr)?
            .and_then(|v| v.downcast_into::<CFBoolean>())
            .map(bool::from))
    }

    pub(super) fn elements(&self, attr: &str, limit: usize) -> Result<Vec<Self>> {
        let strict = attr == "AXWindows";
        let attr = CFString::new(attr);
        let key = attr.as_concrete_TypeRef();
        let mut list = ptr::null();
        let code = unsafe {
            AXUIElementCopyAttributeValues(self.raw(), key, 0, limit as CFIndex, &mut list)
        };
        if let Err(err) = success(code) {
            // A range starting at zero is invalid for a supported empty array.
            if err.0 == -25201 {
                let mut count = -1;
                let count_status =
                    unsafe { AXUIElementGetAttributeValueCount(self.raw(), key, &mut count) };
                if count_status == 0 && count == 0 {
                    return Ok(Vec::new());
                }
            }
            return if err.optional() && (!strict || err.0 == -25212) {
                Ok(Vec::new())
            } else {
                Err(err)
            };
        }
        if list.is_null() {
            return Ok(Vec::new());
        }
        let list: CFArray = unsafe { TCFType::wrap_under_create_rule(list) };
        list.iter()
            .filter_map(|p| Self::from_cf(unsafe { CFType::wrap_under_get_rule(*p) }).transpose())
            .collect()
    }

    pub(super) fn actions(&self) -> Result<Vec<String>> {
        let mut list = ptr::null();
        if let Err(err) = success(unsafe { AXUIElementCopyActionNames(self.raw(), &mut list) }) {
            return if err.optional() {
                Ok(Vec::new())
            } else {
                Err(err)
            };
        }
        if list.is_null() {
            return Ok(Vec::new());
        }
        let list: CFArray<CFString> = unsafe { TCFType::wrap_under_create_rule(list) };
        Ok(list.iter().take(12).map(|s| s.to_string()).collect())
    }

    pub(super) fn press(&self) -> Result<()> {
        success(unsafe {
            AXUIElementPerformAction(self.raw(), CFString::new("AXPress").as_concrete_TypeRef())
        })
    }

    pub(super) fn settable(&self, attr: &str) -> Result<bool> {
        let mut settable = 0;
        let attr = CFString::new(attr);
        match success(unsafe {
            AXUIElementIsAttributeSettable(self.raw(), attr.as_concrete_TypeRef(), &mut settable)
        }) {
            Ok(()) => Ok(settable != 0),
            Err(err) if err.optional() => Ok(false),
            Err(err) => Err(err),
        }
    }

    pub(super) fn select_row(&self, row: &Self) -> Result<()> {
        let attr = CFString::new("AXSelectedRows");
        let rows = CFArray::from_CFTypes(std::slice::from_ref(&row.0));
        success(unsafe {
            AXUIElementSetAttributeValue(
                self.raw(),
                attr.as_concrete_TypeRef(),
                rows.as_CFTypeRef(),
            )
        })
    }

    pub(super) fn set(&self, attr: &str, text: &str) -> Result<()> {
        let attr = CFString::new(attr);
        let value = CFString::new(text);
        success(unsafe {
            AXUIElementSetAttributeValue(
                self.raw(),
                attr.as_concrete_TypeRef(),
                value.as_CFTypeRef(),
            )
        })
    }

    fn ax_value<T>(&self, attr: &str, kind: u32, mut out: T) -> Result<Option<T>> {
        let Some(value) = self.attr(attr)? else {
            return Ok(None);
        };
        if value.type_of() != unsafe { AXValueGetTypeID() } {
            return Ok(None);
        }
        let present = unsafe { AXValueGetValue(value.as_CFTypeRef(), kind, (&raw mut out).cast()) };
        Ok((present != 0).then_some(out))
    }

    pub(super) fn selection(&self) -> Result<Option<CFRange>> {
        self.ax_value(
            "AXSelectedTextRange",
            4,
            CFRange {
                location: 0,
                length: 0,
            },
        )
    }

    pub(super) fn bounds(&self) -> Result<Option<CGRect>> {
        let origin = self.ax_value("AXPosition", 1, CGPoint::new(0.0, 0.0))?;
        let size = self.ax_value("AXSize", 2, CGSize::new(0.0, 0.0))?;
        Ok(origin
            .zip(size)
            .map(|(origin, size)| CGRect::new(&origin, &size)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Process {
    pub(super) pid: i32,
    start: (u64, u64),
}

impl Process {
    pub(super) fn own(pid: i32) -> Option<Self> {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if read != size {
            return None;
        }
        let info = unsafe { info.assume_init() };
        (info.pbi_uid == unsafe { libc::geteuid() }).then_some(Self {
            pid,
            start: (info.pbi_start_tvsec, info.pbi_start_tvusec),
        })
    }

    pub(super) fn live(self) -> bool {
        Self::own(self.pid) == Some(self)
    }
}

pub(super) struct WindowInfo {
    pub(super) id: u32,
    pub(super) pid: i32,
    pub(super) owner: Option<String>,
    pub(super) bounds: CGRect,
}

pub(super) fn windows() -> Option<Vec<WindowInfo>> {
    let list = copy_window_info(kCGWindowListExcludeDesktopElements, 0)?;
    Some(
        list.iter()
            .filter_map(|ptr| {
                let dict: CFDictionary<CFString, CFType> =
                    unsafe { TCFType::wrap_under_get_rule((*ptr).cast()) };
                let number = |key: &str| {
                    dict.find(CFString::new(key))?
                        .downcast::<CFNumber>()?
                        .to_i64()
                };
                if number("kCGWindowLayer")? != 0 {
                    return None;
                }
                let bounds = dict.find(CFString::new("kCGWindowBounds"))?;
                let mut rect = CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(0.0, 0.0));
                if !unsafe {
                    CGRectMakeWithDictionaryRepresentation(bounds.as_CFTypeRef().cast(), &mut rect)
                } {
                    return None;
                }
                Some(WindowInfo {
                    id: number("kCGWindowNumber")?.try_into().ok()?,
                    pid: number("kCGWindowOwnerPID")?.try_into().ok()?,
                    owner: dict
                        .find(CFString::new("kCGWindowOwnerName"))
                        .and_then(|v| v.downcast::<CFString>())
                        .map(|s| s.to_string()),
                    bounds: rect,
                })
            })
            .collect(),
    )
}
