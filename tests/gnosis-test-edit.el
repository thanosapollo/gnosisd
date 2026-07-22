;;; gnosis-test-edit.el --- Thema editor tests  -*- lexical-binding: t; -*-

;;; Code:

(require 'cl-lib)
(require 'ert)
(require 'gnosis-edit)

(defconst gnosis-test-edit--request
  `(:kind "basic" :keimenon "Question\nline two"
    :hypothesis ["guess" ,gnosis-core--json-null]
    :answer ["first" "second\nline"]
    :source_guid ,gnosis-core--json-null
    :parathema "Context" :review_image ,gnosis-core--json-null
    :tags ["logic"]
    :concepts [(:node_id "opaque/node?id" :relation "primary")]
    :asset_paths ["assets/abc/image.png"])
  "Complete thema request used by editor tests.")

(ert-deftest gnosis-edit-json-round-trip-is-lossless ()
  (with-temp-buffer
    (insert (gnosis-edit--render gnosis-test-edit--request))
    (should (equal gnosis-test-edit--request (gnosis-edit--parse)))))

(ert-deftest gnosis-edit-rejects-duplicate-json-fields ()
  (with-temp-buffer
    (insert "{\"kind\":\"basic\",\"kind\":\"cloze\"}")
    (should-error (gnosis-edit--parse) :type 'user-error)))

(ert-deftest gnosis-add-thema-opens-shared-text-editor ()
  (dolist (buffer (list gnosis-edit-new-buffer-name gnosis-edit-buffer-name))
    (when-let* ((live (get-buffer buffer))) (kill-buffer live)))
  (unwind-protect
      (save-window-excursion (gnosis-add-thema))
    (with-current-buffer gnosis-edit-new-buffer-name
      (should (derived-mode-p 'gnosis-edit-mode))
      (should-not gnosis-edit--thema-id)
      (should-not gnosis-edit--revision)
      (should (equal "basic" (plist-get (gnosis-edit--parse) :kind))))
    (when-let* ((buffer (get-buffer gnosis-edit-new-buffer-name)))
      (kill-buffer buffer))))

(ert-deftest gnosis-edit-thema-fetches-before-opening-buffer ()
  (when-let* ((buffer (get-buffer gnosis-edit-buffer-name))) (kill-buffer buffer))
  (cl-letf (((symbol-function 'gnosis-core-thema)
             (lambda (id)
               (should (equal "opaque/thema" id))
               (append '(:thema_id "opaque/thema" :revision 7)
                       gnosis-test-edit--request)))
            ((symbol-function 'gnosis-core-thema-write-view)
             (lambda (_) gnosis-test-edit--request)))
    (unwind-protect
        (save-window-excursion (gnosis-edit-thema "opaque/thema"))
      (with-current-buffer gnosis-edit-buffer-name
        (should (equal "opaque/thema" gnosis-edit--thema-id))
        (should (= 7 gnosis-edit--revision))
        (should (equal gnosis-test-edit--request (gnosis-edit--parse))))
      (when-let* ((buffer (get-buffer gnosis-edit-buffer-name)))
        (kill-buffer buffer)))))

(ert-deftest gnosis-edit-thema-preserves-existing-unsaved-buffer ()
  (let ((buffer (get-buffer-create gnosis-edit-buffer-name))
        (fetches 0))
    (unwind-protect
        (with-current-buffer buffer
          (erase-buffer)
          (insert "unsaved")
          (set-buffer-modified-p t)
          (cl-letf (((symbol-function 'gnosis-core-thema)
                     (lambda (&rest _) (cl-incf fetches))))
            (should-error (gnosis-edit-thema "other") :type 'user-error))
          (should (= fetches 0))
          (should (equal "unsaved" (buffer-string))))
      (kill-buffer buffer))))

(ert-deftest gnosis-edit-save-promotes-create-after-valid-response ()
  (let (calls callback (operation-calls 0))
    (with-temp-buffer
      (gnosis-edit-mode)
      (setq-local gnosis-edit--after-save
                  (lambda (projection) (setq callback projection)))
      (insert (gnosis-edit--render gnosis-test-edit--request))
      (cl-letf (((symbol-function 'gnosis-core-operation-id)
                 (lambda ()
                   (cl-incf operation-calls)
                   "op:save"))
                ((symbol-function 'gnosis-core-thema-create)
                 (lambda (operation-id request)
                   (push (list operation-id request) calls)
                   (append '(:thema_id "created" :revision 1) request)))
                ((symbol-function 'gnosis-core-thema-write-view)
                 (lambda (projection)
                   (cl-loop for (key value) on projection by #'cddr
                            unless (memq key '(:thema_id :revision))
                            append (list key value)))))
        (gnosis-edit-save))
      (should (= 1 (length calls)))
      (should (= 1 operation-calls))
      (should (equal (list "op:save" gnosis-test-edit--request)
                     (car calls)))
      (should (equal "created" gnosis-edit--thema-id))
      (should (= 1 gnosis-edit--revision))
      (should (equal "created" (plist-get callback :thema_id)))
      (should-not (buffer-modified-p)))))

(ert-deftest gnosis-edit-conflict-preserves-buffer-and-revision ()
  (with-temp-buffer
    (gnosis-edit-mode)
    (setq-local gnosis-edit--thema-id "opaque/thema")
    (setq-local gnosis-edit--revision 7)
    (insert (gnosis-edit--render gnosis-test-edit--request))
    (let ((before (buffer-string))
          calls)
      (cl-letf (((symbol-function 'gnosis-core-operation-id)
                 (lambda () "op:update"))
                ((symbol-function 'gnosis-core-thema-update)
                 (lambda (&rest arguments)
                   (push arguments calls)
                   (signal 'gnosis-core-conflict '("stale")))))
        (should-error (gnosis-edit-save) :type 'gnosis-core-conflict))
      (should
       (equal (list (list "opaque/thema" "op:update" 7
                          gnosis-test-edit--request))
              calls))
      (should (= 7 gnosis-edit--revision))
      (should (equal before (buffer-string))))))

(ert-deftest gnosis-edit-uncertain-mutation-locks-blind-retry ()
  (with-temp-buffer
    (gnosis-edit-mode)
    (insert (gnosis-edit--render gnosis-test-edit--request))
    (let ((calls 0)
          operation-ids)
      (cl-letf (((symbol-function 'gnosis-core-operation-id)
                 (lambda ()
                   (push "op:uncertain" operation-ids)
                   "op:uncertain"))
                ((symbol-function 'gnosis-core-thema-create)
                 (lambda (operation-id _request)
                   (should (equal "op:uncertain" operation-id))
                   (cl-incf calls)
                   (signal 'gnosis-core-mutation-uncertain '("uncertain")))))
        (should-error (gnosis-edit-save)
                      :type 'gnosis-core-mutation-uncertain)
        (should-error (gnosis-edit-save) :type 'user-error))
      (should gnosis-edit--write-uncertain)
      (should (equal '("op:uncertain") operation-ids))
      (should (= calls 1)))))

(provide 'gnosis-test-edit)
;;; gnosis-test-edit.el ends here
